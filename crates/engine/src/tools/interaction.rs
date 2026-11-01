//! Conversation questions, plan decisions, and durable wakeup creation.
use super::*;
use xylo_core::{InteractionAnswer, UserQuestion};

#[derive(Clone)]
enum Request {
    Questions(Vec<UserQuestion>),
    Plan,
}
struct Pending {
    request: Request,
    cancel: CancellationToken,
    sender: mpsc::SyncSender<InteractionAnswer>,
}

/// One-shot replies installed before events are emitted and removed on cancellation.
#[derive(Clone, Default)]
pub(crate) struct InteractionGate(Arc<Mutex<std::collections::HashMap<Uuid, Pending>>>);
impl InteractionGate {
    pub(crate) fn answer(
        &self,
        id: Uuid,
        answer: InteractionAnswer,
        permissions: &PermissionCell,
    ) -> Result<()> {
        let mut pending = self.0.lock();
        let entry = pending
            .get(&id)
            .context("This question or plan is no longer waiting")?;
        ensure!(
            !entry.cancel.is_cancelled(),
            "This interaction was cancelled"
        );
        validate_answer(&entry.request, &answer)?;
        let entry = pending.remove(&id).unwrap();
        // Only a validated, current, explicit plan decision can elevate this turn.
        if let InteractionAnswer::ApprovePlan { mode } = &answer {
            permissions.set(*mode);
        }
        entry
            .sender
            .send(answer)
            .context("The turn stopped before receiving your answer")
    }
}
fn validate_answer(request: &Request, answer: &InteractionAnswer) -> Result<()> {
    match (request, answer) {
        (_, InteractionAnswer::Reject) => Ok(()),
        (Request::Plan, InteractionAnswer::ApprovePlan { mode }) => {
            ensure!(*mode != PermissionMode::Plan, "Choose a continuation mode");
            Ok(())
        }
        (Request::Plan, InteractionAnswer::KeepPlanning { feedback }) => {
            ensure!(
                !feedback.trim().is_empty() && feedback.len() <= 8192,
                "Feedback must contain 1..8192 bytes"
            );
            Ok(())
        }
        (Request::Questions(questions), InteractionAnswer::Questions { answers }) => {
            ensure!(questions.len() == answers.len(), "Answer every question");
            for (question, answer) in questions.iter().zip(answers) {
                let other = answer.other.as_deref().unwrap_or("").trim();
                ensure!(other.len() <= 8192, "Other answer is too long");
                let count = answer.selected.len() + usize::from(!other.is_empty());
                ensure!(
                    count > 0 && (question.multi_select || count == 1),
                    "Choose an answer for each question"
                );
                let mut seen = std::collections::HashSet::new();
                ensure!(
                    answer
                        .selected
                        .iter()
                        .all(|index| *index < question.options.len() && seen.insert(*index)),
                    "Invalid or duplicate choice"
                );
            }
            Ok(())
        }
        _ => bail!("Answer does not match this interaction"),
    }
}

pub(super) fn execute(broker: &ToolBroker, id: Uuid, name: &str, args: &Value) -> Result<Value> {
    if name == "schedule_wakeup" {
        let scheduler = xylo_runtime::scheduler::Scheduler::new(
            broker.artifacts.join("schedules.json"),
            broker.request.workspace.clone(),
        )?;
        return Ok(json!(
            broker.runtime.block_on(
                scheduler.create_wakeup(
                    broker.request.session_id,
                    broker.mode(),
                    args["delay_seconds"]
                        .as_u64()
                        .context("delay_seconds must be an integer")?,
                    builtin::text(args, "prompt")?.into(),
                    builtin::text(args, "reason")?.into(),
                    None,
                )
            )?
        ));
    }
    let (request, event) = match name {
        "ask_user" => {
            let questions: Vec<UserQuestion> = serde_json::from_value(args["questions"].clone())?;
            ensure!((1..=4).contains(&questions.len()), "Ask 1–4 questions");
            for question in &questions {
                ensure!(
                    (2..=4).contains(&question.options.len()),
                    "Offer 2–4 options"
                );
                ensure!(
                    !question.question.trim().is_empty()
                        && question.question.len() <= 4096
                        && !question.header.trim().is_empty()
                        && question.header.len() <= 80,
                    "Question or header is empty or too long"
                );
                let mut labels = std::collections::HashSet::new();
                for option in &question.options {
                    ensure!(
                        !option.label.trim().is_empty()
                            && option.label.len() <= 160
                            && option.description.len() <= 1024
                            && !option.label.eq_ignore_ascii_case("other")
                            && labels.insert(option.label.clone()),
                        "Options must have unique labels; Other is automatic"
                    );
                }
            }
            (
                Request::Questions(questions.clone()),
                EngineEvent::QuestionRequired {
                    action_id: id,
                    questions,
                },
            )
        }
        "exit_plan_mode" => {
            let plan = builtin::text(args, "plan")?.to_owned();
            ensure!(
                !plan.trim().is_empty() && plan.len() <= 32768,
                "Plan must contain 1..32768 bytes"
            );
            (
                Request::Plan,
                EngineEvent::PlanRequired {
                    action_id: id,
                    plan,
                },
            )
        }
        _ => bail!("Unknown interaction tool"),
    };
    if !broker.interactive {
        broker.emit(event)?;
        if let Some(refused) = &broker.on_refusal {
            refused("An interactive question or plan was declined in headless mode");
        }
        return Ok(
            json!({"answer":{"kind":"reject"},"reason":"No interactive client is attached"}),
        );
    }
    let (sender, receiver) = mpsc::sync_channel(1);
    let gate = &broker.session.interactions;
    gate.0.lock().insert(
        id,
        Pending {
            request: request.clone(),
            cancel: broker.cancel.clone(),
            sender,
        },
    );
    let waiting = broker
        .emit(EngineEvent::Phase {
            phase: TurnPhase::WaitingForApproval,
        })
        .and_then(|()| broker.emit(event));
    if let Err(error) = waiting {
        gate.0.lock().remove(&id);
        return Err(error);
    }
    let answer = loop {
        if broker.cancel.is_cancelled() {
            break None;
        }
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(answer) => break Some(answer),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => break None,
        }
    };
    gate.0.lock().remove(&id);
    let _ = broker.emit(EngineEvent::Phase {
        phase: TurnPhase::RunningTools,
    });
    ensure!(
        !broker.cancel.is_cancelled(),
        "Interaction cancelled with the turn"
    );
    let answer = answer.context("Interaction client disconnected")?;
    let resolved = match (&request, &answer) {
        (Request::Questions(questions), InteractionAnswer::Questions { answers }) => questions.iter().zip(answers).map(|(question, answer)| json!({"question":question.question,"selected":answer.selected.iter().map(|i| &question.options[*i].label).collect::<Vec<_>>(),"other":answer.other})).collect::<Vec<_>>(),
        _ => Vec::new(),
    };
    Ok(json!({"answer":answer,"resolved_answers":resolved}))
}

pub(super) fn definitions() -> Vec<ToolDefinition> {
    let make = |name: &str, description: &str, properties: Value, required: Value, writes| {
        ToolDefinition {
            name: name.into(),
            description: description.into(),
            writes,
            parameters: json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}),
        }
    };
    vec![
        make(
            "ask_user",
            "Ask 1–4 focused questions with 2–4 options each. Other free text is always available. Waits for answers or cancellation.",
            json!({"questions":{"type":"array","minItems":1,"maxItems":4,"items":{"type":"object","properties":{"question":{"type":"string","minLength":1,"maxLength":4096},"header":{"type":"string","minLength":1,"maxLength":80},"multi_select":{"type":"boolean"},"options":{"type":"array","minItems":2,"maxItems":4,"items":{"type":"object","properties":{"label":{"type":"string","minLength":1,"maxLength":160},"description":{"type":"string","maxLength":1024}},"required":["label","description"],"additionalProperties":false}}},"required":["question","header","options","multi_select"],"additionalProperties":false}}}),
            json!(["questions"]),
            true,
        ),
        make(
            "exit_plan_mode",
            "Present your concrete plan for user approval. Approval selects the continuation mode; keep planning supplies feedback; rejection grants no authority.",
            json!({"plan":{"type":"string","minLength":1,"maxLength":32768}}),
            json!(["plan"]),
            true,
        ),
        make(
            "schedule_wakeup",
            "Resume this same conversation later with a visible reason and the current permission limit. Delay is 60–3600 seconds. Requires permission for scheduling work.",
            json!({"delay_seconds":{"type":"integer","minimum":60,"maximum":3600},"prompt":{"type":"string","minLength":1,"maxLength":8192},"reason":{"type":"string","minLength":1,"maxLength":1024}}),
            json!(["delay_seconds", "prompt", "reason"]),
            true,
        ),
    ]
}
