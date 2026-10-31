use super::*;
#[test]
fn lifecycle_routes_durable_ids_and_generic_kind_without_prompt_text() {
    let (sender, receiver) = async_channel::bounded(4);
    let foreground = AtomicBool::new(false);
    let session = Uuid::new_v4();
    let turn = Uuid::new_v4();
    let action = Uuid::new_v4();
    emit(
        &sender,
        &foreground,
        session,
        turn,
        &EngineEvent::ApprovalRequired {
            action_id: action,
            name: "private-tool".into(),
            arguments: serde_json::json!({"secret":"private-content"}),
            rule: None,
            parent_id: None,
            reason: None,
            rule_key: None,
        },
        false,
    );
    let event = receiver.try_recv().unwrap();
    assert_eq!(event.session_id, session);
    assert_eq!(event.turn_id, turn);
    assert_eq!(event.event_id, action);
    assert!(event.background);
    assert_eq!(event.kind, Kind::Approval);
    foreground.store(true, Ordering::Release);
    emit(
        &sender,
        &foreground,
        session,
        turn,
        &EngineEvent::Completed {
            text: "private reply".into(),
            usage: Default::default(),
        },
        true,
    );
    let event = receiver.try_recv().unwrap();
    assert_eq!(event.kind, Kind::ScheduleCompleted);
    assert!(!event.background);
    emit(
        &sender,
        &foreground,
        session,
        turn,
        &EngineEvent::Delta {
            text: "stream".into(),
        },
        false,
    );
    assert!(receiver.try_recv().is_err());
}
