//! Harness invariants every trial must keep, whatever the model did.
//!
//! These are properties of the engine, not of the model's answer: a run can
//! solve its task and still violate them, and reports count such runs
//! separately. Each invariant restates documented engine behaviour:
//! - one terminal event per turn, followed at most by a generated title;
//! - every started tool reports exactly one finish;
//! - no request pairs a tool call with a missing, duplicated or stray result;
//! - the system prompt and tool definitions are byte-identical across the
//!   requests of each conversation in a turn (the agent's and every
//!   delegated child's), so the provider's prompt cache keeps hitting;
//! - every fitted request leaves the output reserve free;
//! - the turn's reported usage is the sum of its agent requests' usage;
//! - the action journal is settled when the turn ends;
//! - an action the user denied never runs;
//! - a cancelled turn ends its event stream within the runner's grace time.

use crate::{
    runner::Trial,
    task::Check,
    trace::{Exchange, Outcome},
};
use std::collections::{HashMap, HashSet};
use xylo_core::EngineEvent;
use xylo_engine::{Message, Role};

/// Checks every invariant on `trial`.
pub fn check(trial: &Trial) -> Vec<Check> {
    vec![
        single_terminal_event(trial),
        tool_events_paired(trial),
        requests_well_formed(trial),
        stable_prefix(trial),
        requests_fit_window(trial),
        usage_accounted(trial),
        journal_settled(trial),
        denials_respected(trial),
        cancellation_honoured(trial),
    ]
}

fn terminal(event: &EngineEvent) -> bool {
    matches!(
        event,
        EngineEvent::Completed { .. } | EngineEvent::Cancelled | EngineEvent::Error { .. }
    )
}

fn single_terminal_event(trial: &Trial) -> Check {
    const NAME: &str = "single_terminal_event";
    let events = &trial.trace.events;
    let terminals: Vec<usize> = (0..events.len())
        .filter(|&index| terminal(&events[index]))
        .collect();
    let [index] = terminals[..] else {
        return Check::expect(NAME, false, format!("{} terminal events", terminals.len()));
    };
    let trailing = events[index + 1..]
        .iter()
        .find(|event| !matches!(event, EngineEvent::TitleUpdated { .. }));
    Check::expect(
        NAME,
        trailing.is_none(),
        match trailing {
            Some(event) => format!("event after the terminal event: {}", event_name(event)),
            None => "one terminal event, last".into(),
        },
    )
}

fn tool_events_paired(trial: &Trial) -> Check {
    let mut open: HashMap<uuid::Uuid, (u32, u32)> = HashMap::new();
    for event in &trial.trace.events {
        match event {
            EngineEvent::ToolStarted { action_id, .. } => {
                open.entry(*action_id).or_default().0 += 1
            }
            EngineEvent::ToolFinished { action_id, .. } => {
                open.entry(*action_id).or_default().1 += 1;
            }
            _ => {}
        }
    }
    let unpaired = open.iter().filter(|(_, counts)| **counts != (1, 1)).count();
    Check::expect(
        "tool_events_paired",
        unpaired == 0,
        format!(
            "{} actions, {unpaired} without exactly one start and one finish",
            open.len()
        ),
    )
}

fn requests_well_formed(trial: &Trial) -> Check {
    const NAME: &str = "requests_well_formed";
    for (number, exchange) in trial.trace.agent_exchanges().enumerate() {
        if let Err(problem) = well_formed(&exchange.request.messages) {
            return Check::expect(
                NAME,
                false,
                format!("agent request {}: {problem}", number + 1),
            );
        }
    }
    Check::expect(NAME, true, "every call answered once, in place")
}

/// Whether a request's messages pair each tool call with exactly one result
/// directly after the calling message.
pub(crate) fn well_formed(messages: &[Message]) -> Result<(), String> {
    if messages.first().map(|message| message.role) != Some(Role::System) {
        return Err("does not start with the system prompt".into());
    }
    let mut ids = HashSet::new();
    let mut index = 1;
    while index < messages.len() {
        let message = &messages[index];
        index += 1;
        match message.role {
            Role::Tool => return Err("a tool result does not follow its call".into()),
            Role::Assistant if !message.tool_calls.is_empty() => {
                let mut expected = HashSet::new();
                for call in &message.tool_calls {
                    if !ids.insert(call.id.as_str()) || !expected.insert(call.id.as_str()) {
                        return Err(format!("call id {} is reused", call.id));
                    }
                }
                while index < messages.len() && messages[index].role == Role::Tool {
                    let answered = messages[index].tool_call_id.as_deref().unwrap_or_default();
                    if !expected.remove(answered) {
                        return Err(format!("result for unknown or answered call {answered}"));
                    }
                    index += 1;
                }
                if let Some(missing) = expected.into_iter().next() {
                    return Err(format!("call {missing} has no result"));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn stable_prefix(trial: &Trial) -> Check {
    const NAME: &str = "stable_prefix";
    let root: Vec<&Exchange> = trial.trace.agent_exchanges().collect();
    if root.is_empty() {
        return Check::skip(NAME, "no agent requests");
    }
    let children = trial.trace.child_conversations();
    let conversations = std::iter::once(("the agent".to_owned(), root)).chain(
        children.into_iter().map(|(task, exchanges)| {
            (
                format!("child {:?}", task.lines().next().unwrap_or_default()),
                exchanges,
            )
        }),
    );
    let mut checked = 0;
    for (whose, exchanges) in conversations {
        checked += 1;
        let first = &exchanges[0].request;
        for (number, exchange) in exchanges.iter().enumerate().skip(1) {
            let request = &exchange.request;
            let changed = if request.system() != first.system() {
                "system prompt"
            } else if request.tools_digest != first.tools_digest {
                "tool definitions"
            } else {
                continue;
            };
            return Check::expect(
                NAME,
                false,
                format!("{changed} of {whose} changed at its request {}", number + 1),
            );
        }
    }
    Check::expect(
        NAME,
        true,
        format!("system prompt and tools identical across the requests of {checked} conversations"),
    )
}

fn cancellation_honoured(trial: &Trial) -> Check {
    const NAME: &str = "cancellation_honoured";
    if !trial.timed_out {
        return Check::skip(NAME, "the trial was not cancelled");
    }
    Check::expect(
        NAME,
        !trial.hung,
        if trial.hung {
            "the engine kept its event stream open after cancellation"
        } else {
            "the engine ended the turn after cancellation"
        },
    )
}

fn requests_fit_window(trial: &Trial) -> Check {
    const NAME: &str = "requests_fit_window";
    let mut fitted = 0;
    for event in &trial.trace.events {
        let EngineEvent::ContextUsage { usage } = event else {
            continue;
        };
        fitted += 1;
        let room = usage.capacity.saturating_sub(usage.output_reserve);
        if usage.used() > room {
            return Check::expect(
                NAME,
                false,
                format!("{} tokens sent with room for {room}", usage.used()),
            );
        }
    }
    if fitted == 0 {
        return Check::skip(NAME, "no context usage reported");
    }
    Check::expect(NAME, true, format!("{fitted} requests within the window"))
}

fn usage_accounted(trial: &Trial) -> Check {
    const NAME: &str = "usage_accounted";
    let Some(EngineEvent::Completed { usage, .. }) = trial.terminal() else {
        return Check::skip(NAME, "turn did not complete");
    };
    if trial.actions("agent_delegate").next().is_some() {
        return Check::skip(NAME, "delegated work reports its own usage");
    }
    let (mut input, mut output) = (0, 0);
    for exchange in trial.trace.agent_exchanges() {
        let Outcome::Reply(reply) = &exchange.outcome else {
            continue;
        };
        let Some(reported) = &reply.usage else {
            return Check::skip(NAME, "a reply reported no usage");
        };
        input += crate::metrics::count(reported, "input_tokens");
        output += crate::metrics::count(reported, "output_tokens");
    }
    Check::expect(
        NAME,
        (usage.input_tokens, usage.output_tokens) == (input, output),
        format!(
            "turn reported {}/{} input/output tokens, requests sum to {input}/{output}",
            usage.input_tokens, usage.output_tokens
        ),
    )
}

fn journal_settled(trial: &Trial) -> Check {
    Check::expect(
        "journal_settled",
        trial.unresolved_actions == 0,
        format!("{} unresolved actions", trial.unresolved_actions),
    )
}

fn denials_respected(trial: &Trial) -> Check {
    const NAME: &str = "denials_respected";
    let denied: HashSet<uuid::Uuid> = trial
        .trace
        .events
        .iter()
        .zip(denial_flags(trial))
        .filter_map(|(event, denied)| match event {
            EngineEvent::ApprovalRequired { action_id, .. } if denied => Some(*action_id),
            _ => None,
        })
        .collect();
    if denied.is_empty() {
        return Check::skip(NAME, "nothing was denied");
    }
    let ran = trial.trace.events.iter().any(|event| {
        matches!(event, EngineEvent::ToolFinished { action_id, success: true, .. } if denied.contains(action_id))
    }) || trial
        .trace
        .actions
        .iter()
        .any(|action| denied.contains(&action.id) && action.state == "succeeded");
    Check::expect(
        NAME,
        !ran,
        format!("{} denied actions; any ran: {ran}", denied.len()),
    )
}

/// For each event, whether it is an approval request the user denied.
fn denial_flags(trial: &Trial) -> Vec<bool> {
    let mut decisions = trial.approvals.iter();
    trial
        .trace
        .events
        .iter()
        .map(|event| match event {
            EngineEvent::ApprovalRequired { .. } => {
                decisions.next().is_some_and(|decision| !decision.allowed)
            }
            _ => false,
        })
        .collect()
}

fn event_name(event: &EngineEvent) -> String {
    serde_json::to_value(event)
        .ok()
        .and_then(|value| {
            value
                .get("event")
                .and_then(|name| name.as_str().map(str::to_owned))
        })
        .unwrap_or_else(|| "event".into())
}

#[cfg(test)]
mod tests;
