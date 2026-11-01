//! Interaction events attach to the original call and settle on cancellation.
use super::*;
#[test]
fn interaction_question_card_settles_with_its_turn() {
    let mut state = TurnState::default();
    state.apply(EngineEvent::Started);
    let id = Uuid::new_v4();
    started(&mut state, id, "ask_user", None);
    state.apply(EngineEvent::QuestionRequired {
        action_id: id,
        questions: vec![xylo_core::UserQuestion {
            question: "Scope?".into(),
            header: "Scope".into(),
            multi_select: false,
            options: vec![],
        }],
    });
    assert_eq!(state.tools.len(), 1);
    assert!(state.tools[0].approval);
    assert_eq!(state.tools[0].question_answers.len(), 1);
    state.apply(EngineEvent::Cancelled);
    assert!(!state.tools[0].approval);
}
#[test]
fn interaction_plan_card_reuses_the_original_call() {
    let mut state = TurnState::default();
    state.apply(EngineEvent::Started);
    let id = Uuid::new_v4();
    started(&mut state, id, "exit_plan_mode", None);
    state.apply(EngineEvent::PlanRequired {
        action_id: id,
        plan: "Inspect then edit one file".into(),
    });
    assert_eq!(state.tools.len(), 1);
    assert_eq!(state.tools[0].input["plan"], "Inspect then edit one file");
    assert!(state.tools[0].approval);
    finished(&mut state, id, "exit_plan_mode", "rejected", true);
    assert!(!state.tools[0].approval);
}
