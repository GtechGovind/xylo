use super::{answer_segment, duration_label, group_summary, has_work, latest_step, phase_label};
use crate::state::{ToolCard, TurnState};
use uuid::Uuid;
use xylo_core::{EngineEvent, TurnPhase};

fn card(name: &str) -> ToolCard {
    ToolCard {
        id: Uuid::new_v4(),
        name: name.into(),
        input: serde_json::json!({}),
        result: None,
        presentation: Default::default(),
        approval: false,
        rule: None,
        reason: None,
        parent_id: None,
        success: Some(true),
        not_completed: false,
        decision_lost: false,
        started_at: 0,
        finished_at: None,
    }
}

#[test]
fn summary_counts_categories_in_first_seen_order() {
    let tools = [
        card("file_read"),
        card("search_project"),
        card("file_read"),
        card("run_command"),
        card("find_files"),
        card("file_read"),
    ];
    assert_eq!(
        group_summary(&tools),
        "Read 3 files · Searched 2 patterns · Ran 1 command"
    );
    assert_eq!(group_summary(&[card("custom")]), "Used 1 tool");
}

#[test]
fn error_detail_renders_literally() {
    assert_eq!(super::literal_markdown("a_b *c*"), "a\\_b \\*c\\*");
    assert_eq!(super::literal_markdown("one\ntwo"), "one  \ntwo");
}

#[test]
fn durations_switch_to_minutes() {
    assert_eq!(duration_label(12_400), "12s");
    assert_eq!(duration_label(64_000), "1m 04s");
    assert_eq!(duration_label(-5), "0s");
}

#[test]
fn activity_follows_phase_and_waiting_approvals() {
    let mut turn = TurnState::default();
    turn.apply(EngineEvent::Started);
    assert_eq!(phase_label(&turn), "Preparing…");
    turn.apply(EngineEvent::Phase {
        phase: TurnPhase::Compacting,
    });
    assert_eq!(phase_label(&turn), "Condensing earlier context…");
    turn.apply(EngineEvent::Phase {
        phase: TurnPhase::Retrying {
            attempt: 2,
            delay_ms: 12_000,
        },
    });
    assert!(phase_label(&turn).ends_with("(attempt 2)"));
    assert!(phase_label(&turn).starts_with("Retrying in 1"));
    turn.apply(EngineEvent::ApprovalRequired {
        action_id: Uuid::new_v4(),
        name: "file_edit".into(),
        arguments: serde_json::json!({}),
        rule: None,
        rule_key: None,
        reason: None,
        parent_id: None,
    });
    assert_eq!(phase_label(&turn), "Waiting for your approval");
}

#[test]
fn a_finished_turn_ends_in_its_answer_and_folds_the_steps_before_it() {
    let mut turn = TurnState::default();
    turn.apply(EngineEvent::Started);
    turn.apply(EngineEvent::Delta {
        text: "Let me look.".into(),
    });
    let read = Uuid::new_v4();
    turn.apply(EngineEvent::ToolStarted {
        action_id: read,
        name: "file_read".into(),
        arguments: serde_json::json!({"path": "src/main.rs"}),
        parent_id: None,
    });
    assert_eq!(
        answer_segment(&turn),
        None,
        "A running turn has no answer yet"
    );
    assert_eq!(
        latest_step(&turn),
        None,
        "A top-level call shows its own running row"
    );
    turn.apply(EngineEvent::ToolFinished {
        action_id: read,
        name: "file_read".into(),
        result: "{}".into(),
        success: true,
        parent_id: None,
    });
    assert_eq!(latest_step(&turn), None, "Nothing is in flight");
    turn.apply(EngineEvent::Completed {
        text: "It starts in main.".into(),
        usage: Default::default(),
    });
    assert_eq!(answer_segment(&turn), Some(2));
    assert!(has_work(&turn));

    let mut plain = TurnState::default();
    plain.apply(EngineEvent::Started);
    plain.apply(EngineEvent::Completed {
        text: "Hello!".into(),
        usage: Default::default(),
    });
    assert_eq!(answer_segment(&plain), Some(0));
    assert!(!has_work(&plain), "A plain reply has nothing to summarise");
}

#[test]
fn the_activity_line_names_a_helper_agents_call_on_one_line() {
    let mut turn = TurnState::default();
    turn.apply(EngineEvent::Started);
    turn.apply(EngineEvent::ToolStarted {
        action_id: Uuid::new_v4(),
        name: "search_project".into(),
        arguments: serde_json::json!({"query": "first\nsecond"}),
        parent_id: Some(Uuid::new_v4()),
    });
    assert_eq!(latest_step(&turn).as_deref(), Some("Search first second"));
}

#[test]
fn harness_notes_alone_are_not_work_to_fold() {
    let mut turn = TurnState::default();
    turn.apply(EngineEvent::Started);
    turn.apply(EngineEvent::Delta {
        text: "Part one".into(),
    });
    turn.apply(EngineEvent::HarnessNote {
        kind: xylo_core::NoteKind::Continuation,
        text: "Asked the model to continue.".into(),
    });
    turn.apply(EngineEvent::Delta {
        text: " and two".into(),
    });
    turn.apply(EngineEvent::Completed {
        text: "Part one and two".into(),
        usage: Default::default(),
    });
    assert_eq!(answer_segment(&turn), Some(1));
    assert!(
        !has_work(&turn),
        "A plain answer gets no 'Worked for' header for a note"
    );
}
