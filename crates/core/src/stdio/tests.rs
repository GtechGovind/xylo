use super::*;
#[test]
fn protocol_rejects_other_versions_and_unknown_commands() {
    assert!(
        serde_json::from_str::<Input>(
            r#"{"version":1,"payload":{"command":"unlock","key":"secret"}}"#
        )
        .is_err()
    );
    let input: Input =
        serde_json::from_str(r#"{"version":4,"payload":{"command":"run","prompt":"hello"}}"#)
            .unwrap();
    assert!(input.validate().is_err());
}
#[test]
fn event_round_trip_preserves_identity_and_sequence() {
    let event = Event {
        version: VERSION,
        session_id: uuid::Uuid::new_v4(),
        turn_id: uuid::Uuid::new_v4(),
        sequence: 4,
        exit_code: None,
        payload: xylo_core_event(),
    };
    let decoded: Event = serde_json::from_slice(&serde_json::to_vec(&event).unwrap()).unwrap();
    decoded.validate().unwrap();
    assert_eq!(decoded.sequence, 4);
    assert_eq!(decoded.session_id, event.session_id);
}
fn xylo_core_event() -> crate::EngineEvent {
    crate::EngineEvent::Started
}
