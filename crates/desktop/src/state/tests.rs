use super::*;
use serde_json::json;

fn started(state: &mut TurnState, id: Uuid, name: &str, parent_id: Option<Uuid>) {
    state.apply(EngineEvent::ToolStarted {
        action_id: id,
        name: name.into(),
        arguments: json!({"task": "Review navigation", "path": "a.rs"}),
        parent_id,
    });
}
fn finished(state: &mut TurnState, id: Uuid, name: &str, result: &str, success: bool) {
    state.apply(EngineEvent::ToolFinished {
        action_id: id,
        name: name.into(),
        result: result.into(),
        success,
        parent_id: None,
    });
}
fn delta(state: &mut TurnState, text: &str) {
    state.apply(EngineEvent::Delta { text: text.into() });
}

#[test]
fn approval_and_finish_share_one_tool_card() {
    let mut state = TurnState::default();
    state.apply(EngineEvent::Started);
    let id = Uuid::new_v4();
    started(&mut state, id, "write_file", None);
    state.apply(EngineEvent::ApprovalRequired {
        action_id: id,
        name: "write_file".into(),
        arguments: json!({}),
        rule: Some("edits".into()),
        parent_id: None,
        reason: Some("Writes a file".into()),
        rule_key: None,
    });
    assert_eq!(state.tools.len(), 1);
    assert!(state.tools[0].approval);
    assert_eq!(state.tools[0].reason.as_deref(), Some("Writes a file"));
    assert_eq!(state.segments, vec![Segment::Tools(vec![id])]);
    finished(&mut state, id, "write_file", "saved", true);
    assert!(!state.tools[0].approval);
    assert_eq!(state.tools[0].success, Some(true));
}

#[test]
fn an_undelivered_decision_settles_the_waiting_card() {
    let mut state = TurnState::default();
    state.apply(EngineEvent::Started);
    let id = Uuid::new_v4();
    state.apply(EngineEvent::ApprovalRequired {
        action_id: id,
        name: "file_edit".into(),
        arguments: json!({}),
        rule: None,
        rule_key: None,
        reason: None,
        parent_id: None,
    });
    finished(
        &mut state,
        id,
        "file_edit",
        "The task is no longer running, so this decision was not applied.",
        false,
    );
    let card = state.tool(id).unwrap();
    assert!(!card.approval && card.decision_lost);
    assert_eq!(state.pending_approvals().count(), 0);
    assert!(card.presentation.error.is_some(), "The reason shows inline");
}

#[test]
fn text_and_tool_batches_stay_in_event_order() {
    let mut state = TurnState::default();
    state.apply(EngineEvent::Started);
    delta(&mut state, "Looking ");
    delta(&mut state, "around.");
    let (read, search, edit) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    started(&mut state, read, "file_read", None);
    started(&mut state, search, "search_project", None);
    delta(&mut state, "Found it.");
    started(&mut state, edit, "file_edit", None);
    delta(&mut state, "Done");
    assert_eq!(
        state.segments,
        vec![
            Segment::Text("Looking around.".into()),
            Segment::Tools(vec![read, search]),
            Segment::Text("Found it.".into()),
            Segment::Tools(vec![edit]),
            Segment::Text("Done".into()),
        ]
    );
}

#[test]
fn completion_replaces_the_streamed_answer_in_place() {
    let mut state = TurnState::default();
    state.apply(EngineEvent::Started);
    delta(&mut state, "Narration");
    let read = Uuid::new_v4();
    started(&mut state, read, "file_read", None);
    delta(&mut state, "Final answ");
    state.apply(EngineEvent::Completed {
        text: "Final answer.".into(),
        usage: Usage::default(),
    });
    assert_eq!(state.segments.len(), 3);
    assert_eq!(state.segments[2], Segment::Text("Final answer.".into()));
    assert_eq!(state.outcome, Some(TurnOutcome::Completed));
    assert_eq!(
        state.reply(),
        Some(("Final answer.".into(), MessageStatus::Complete))
    );
    // A tool-only turn has no reply row at all.
    let mut tools_only = TurnState::default();
    tools_only.apply(EngineEvent::Started);
    started(&mut tools_only, Uuid::new_v4(), "file_read", None);
    tools_only.apply(EngineEvent::Completed {
        text: String::new(),
        usage: Usage::default(),
    });
    assert!(
        !tools_only
            .segments
            .iter()
            .any(|segment| matches!(segment, Segment::Text(_)))
    );
    assert_eq!(tools_only.reply(), None);
}

#[test]
fn cancellation_preserves_partial_output_and_marks_stopped() {
    let mut state = TurnState::default();
    state.apply(EngineEvent::Started);
    delta(&mut state, "partial");
    state.apply(EngineEvent::Cancelled);
    assert_eq!(state.transcript_text(), "partial");
    assert!(!state.running);
    assert!(state.stopped());
    assert_eq!(
        state.reply(),
        Some(("partial".into(), MessageStatus::Interrupted))
    );
}

#[test]
fn errors_keep_partial_text_and_their_classification() {
    let mut state = TurnState::default();
    state.apply(EngineEvent::Started);
    delta(&mut state, "partial");
    state.apply(EngineEvent::Error {
        message: "Rate limited".into(),
        retryable: true,
        kind: ErrorKind::RateLimited,
    });
    assert_eq!(state.transcript_text(), "partial");
    let failure = state.failure().unwrap();
    assert_eq!(failure.kind, ErrorKind::RateLimited);
    assert!(failure.retryable);
    assert_eq!(state.reply().unwrap().1, MessageStatus::Failed);
}

#[test]
fn terminal_events_settle_unfinished_cards_and_helpers() {
    let mut state = TurnState::default();
    state.apply(EngineEvent::Started);
    let (running, waiting, helper) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    started(&mut state, running, "run_command", None);
    state.apply(EngineEvent::ApprovalRequired {
        action_id: waiting,
        name: "file_edit".into(),
        arguments: json!({}),
        rule: None,
        rule_key: None,
        reason: None,
        parent_id: None,
    });
    started(&mut state, helper, DELEGATE_TOOL, None);
    state.apply(EngineEvent::Error {
        message: "worker exited".into(),
        retryable: false,
        kind: ErrorKind::Internal,
    });
    assert!(
        state
            .tools
            .iter()
            .all(|tool| !tool.approval && tool.not_completed)
    );
    assert_eq!(state.subagents[0].state, SubagentState::NotCompleted);
    assert!(state.subagents[0].finished_at.is_some());
    assert_eq!(state.phase, None);
}

#[test]
fn helper_calls_fold_under_their_subagent_row() {
    let mut state = TurnState::default();
    state.apply(EngineEvent::Started);
    let (helper, child, sibling) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    started(&mut state, helper, DELEGATE_TOOL, None);
    started(&mut state, child, "file_read", Some(helper));
    started(&mut state, sibling, "file_read", None);
    assert_eq!(
        state.segments,
        vec![Segment::Subagent(helper), Segment::Tools(vec![sibling])]
    );
    let agent = state.subagent(helper).unwrap();
    assert_eq!(agent.task, "Review navigation");
    assert_eq!(agent.tools, vec![child]);
    assert_eq!(state.tool(child).unwrap().parent_id, Some(helper));
    finished(
        &mut state,
        helper,
        DELEGATE_TOOL,
        &json!({"status":"success","summary":{"response":"## Findings\nUse a sidebar."}})
            .to_string(),
        true,
    );
    let agent = state.subagent(helper).unwrap();
    assert_eq!(agent.state, SubagentState::Completed);
    assert!(agent.summary.as_deref().unwrap().starts_with("## Findings"));
}

#[test]
fn a_forked_skill_becomes_a_helper_row_in_place() {
    let mut state = TurnState::default();
    state.apply(EngineEvent::Started);
    let (before, skill, child, after) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    started(&mut state, before, "file_read", None);
    state.apply(EngineEvent::ToolStarted {
        action_id: skill,
        name: "extension_skill".into(),
        arguments: json!({"id": "code-review"}),
        parent_id: None,
    });
    started(&mut state, after, "file_read", None);
    started(&mut state, child, "search_project", Some(skill));
    assert_eq!(
        state.segments,
        vec![
            Segment::Tools(vec![before]),
            Segment::Subagent(skill),
            Segment::Tools(vec![after]),
        ]
    );
    let agent = state.subagent(skill).unwrap();
    assert_eq!(agent.task, "Code review");
    assert_eq!(agent.tools, vec![child]);
}

#[test]
fn only_the_newest_successful_plan_is_current() {
    let mut state = TurnState::default();
    state.apply(EngineEvent::Started);
    let (first, second) = (Uuid::new_v4(), Uuid::new_v4());
    started(&mut state, first, PLAN_TOOL, None);
    finished(&mut state, first, PLAN_TOOL, "{}", true);
    started(&mut state, second, PLAN_TOOL, None);
    assert_eq!(state.plan, Some(second));
    finished(&mut state, second, PLAN_TOOL, "{\"error\":\"bad\"}", false);
    assert_eq!(state.plan, Some(first));
}

#[test]
fn admission_phase_opens_the_turn_that_started_continues() {
    let mut state = TurnState::default();
    // A late phase after a finished turn never reopens it.
    state.apply(EngineEvent::Phase {
        phase: TurnPhase::Thinking,
    });
    assert!(!state.running);
    state.apply(EngineEvent::Phase {
        phase: TurnPhase::Preparing,
    });
    assert!(state.running);
    let opened = state.started_at;
    state.apply(EngineEvent::Started);
    assert_eq!(state.started_at, opened);
    state.apply(EngineEvent::Phase {
        phase: TurnPhase::Retrying {
            attempt: 2,
            delay_ms: 12_000,
        },
    });
    assert!(matches!(
        state.phase,
        Some(TurnPhase::Retrying { attempt: 2, .. })
    ));
}

#[test]
fn a_refused_admission_clears_the_preparing_turn() {
    let mut state = TurnState::default();
    state.apply(EngineEvent::Phase {
        phase: TurnPhase::Preparing,
    });
    assert!(state.running);
    state.abandon_admission();
    assert!(!state.running && state.phase.is_none() && state.segments.is_empty());
    // A turn that already produced output is never abandoned.
    state.apply(EngineEvent::Started);
    delta(&mut state, "working");
    state.abandon_admission();
    assert!(state.running);
}

#[test]
fn a_new_turn_archives_the_previous_transcript_and_keeps_helpers() {
    let mut state = TurnState::default();
    state.apply(EngineEvent::Started);
    started(&mut state, Uuid::new_v4(), DELEGATE_TOOL, None);
    state.apply(EngineEvent::Completed {
        text: "Done".into(),
        usage: Usage::default(),
    });
    state.apply(EngineEvent::Started);
    assert!(state.segments.is_empty() && state.tools.is_empty());
    assert!(state.subagents.is_empty());
    assert_eq!(state.earlier_subagents.len(), 1);
    assert_eq!(state.outcome, None);
}
