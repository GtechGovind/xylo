use super::*;
use crate::assistant_reply_id;
use xylo_core::{Conversation, pricing::CostEstimate};

#[test]
fn usage_reports_scope_answers_and_days_without_repricing_receipts() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(root.path().join("db")).unwrap();
    let session = Uuid::new_v4();
    let other = Uuid::new_v4();
    for id in [session, other] {
        store
            .upsert_conversation(&Conversation {
                id,
                title: "Usage".into(),
                workspace: root.path().into(),
                updated_at: 0,
                title_source: Default::default(),
                pinned: false,
                archived: false,
            })
            .unwrap();
    }
    let turns = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
    for (index, (chat, turn)) in [(session, turns[0]), (session, turns[1]), (other, turns[2])]
        .into_iter()
        .enumerate()
    {
        store
            .complete_turn(
                chat,
                turn,
                &serde_json::json!([]),
                "reply",
                &Usage {
                    input_tokens: 100,
                    output_tokens: 20,
                    cache_read_tokens: Some(30),
                    cache_write_tokens: Some(10),
                    estimated_cost: Some(CostEstimate {
                        usd: (index + 1) as f64,
                        unpriced_calls: 0,
                    }),
                    ..Default::default()
                },
            )
            .unwrap();
    }
    store
        .call(move |connection| {
            for (index, turn) in turns.into_iter().enumerate() {
                connection.execute(
                    "UPDATE completed_turns SET completed_at=?1 WHERE turn_id=?2",
                    rusqlite::params![if index == 0 { 1 } else { 15 }, turn.to_string()],
                )?;
            }
            Ok(())
        })
        .unwrap();
    let mut settings = store.settings().unwrap();
    settings.pricing_overrides.insert(
        "new price".into(),
        xylo_core::pricing::ModelPrice::default(),
    );
    store.save_settings(&settings).unwrap();
    let report = store
        .usage_report(
            session,
            vec![assistant_reply_id(turns[0]), assistant_reply_id(turns[2])],
            10,
            20,
        )
        .unwrap();
    assert_eq!(
        report.messages.len(),
        1,
        "Answers from another chat must not leak"
    );
    assert_eq!(
        report.messages[&assistant_reply_id(turns[0])]
            .estimated_cost
            .unwrap()
            .usd,
        1.
    );
    assert_eq!(report.conversation.turns, 2);
    assert_eq!(report.conversation.input_tokens, 200);
    assert_eq!(report.conversation.cache_read_tokens, 60);
    assert_eq!(report.conversation.cache_write_tokens, 20);
    assert_eq!(report.conversation.estimated_usd, 3.);
    assert_eq!(report.daily.turns, 2);
    assert_eq!(report.daily.estimated_usd, 5.);
    assert!(
        store
            .usage_report(session, vec![Uuid::nil(); 501], 10, 20)
            .is_err()
    );
    assert!(store.usage_report(session, vec![], 20, 10).is_err());
}

#[test]
fn interrupted_request_receipts_replace_cumulative_updates_and_survive_reopen() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("db");
    let store = Store::open(&path).unwrap();
    let session = Uuid::new_v4();
    let turn = Uuid::new_v4();
    let call = Uuid::new_v4();
    store
        .upsert_conversation(&Conversation {
            id: session,
            title: "Usage".into(),
            workspace: root.path().into(),
            updated_at: 0,
            title_source: Default::default(),
            pinned: false,
            archived: false,
        })
        .unwrap();
    store.record_turn_usage(session, turn, call, None).unwrap();
    let mut receipt = xylo_core::pricing::UsageCall {
        model: "primary".into(),
        input_tokens: 10,
        output_tokens: 1,
        ..Default::default()
    };
    store
        .record_turn_usage(session, turn, call, Some(receipt.clone()))
        .unwrap();
    receipt.output_tokens = 3;
    store
        .record_turn_usage(session, turn, call, Some(receipt.clone()))
        .unwrap();
    drop(store);
    let reopened = Store::open_existing(&path).unwrap();
    assert_eq!(
        reopened.turn_usage(session, turn).unwrap(),
        vec![(call, Some(receipt))]
    );
    assert!(
        reopened
            .turn_usage(session, Uuid::new_v4())
            .unwrap()
            .is_empty()
    );
    reopened
        .complete_turn(
            session,
            turn,
            &serde_json::json!([]),
            "answer",
            &Usage::default(),
        )
        .unwrap();
    assert!(
        reopened.turn_usage(session, turn).unwrap().is_empty(),
        "A completed receipt replaces pending call rows atomically"
    );
}

#[test]
fn failed_and_running_turn_receipts_appear_in_totals_without_double_counting() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("db");
    let store = Store::open(&path).unwrap();
    let session = Uuid::new_v4();
    let other = Uuid::new_v4();
    let turns = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
    for (chat, turn, timestamp) in [
        (session, turns[0], 15),
        (session, turns[1], 1),
        (other, turns[2], 15),
    ] {
        store
            .upsert_conversation(&Conversation {
                id: chat,
                title: "Usage".into(),
                workspace: root.path().into(),
                updated_at: timestamp,
                title_source: Default::default(),
                pinned: false,
                archived: false,
            })
            .unwrap();
        store
            .append_message(&xylo_core::ChatMessage {
                activity_count: 0,
                activity_summary: None,
                turn_id: None,
                id: Uuid::new_v5(&turn, b"user-message"),
                session_id: chat,
                role: "user".into(),
                content: "request".into(),
                created_at: timestamp,
                attachments: vec![],
                status: Default::default(),
                reasoning: None,
            })
            .unwrap();
        store
            .record_turn_usage(
                chat,
                turn,
                Uuid::new_v4(),
                Some(xylo_core::pricing::UsageCall {
                    model: "unpriced-fixture".into(),
                    input_tokens: 100,
                    output_tokens: 20,
                    cache_read_tokens: 30,
                    cache_write_tokens: 10,
                }),
            )
            .unwrap();
    }
    store
        .record_interrupted_reply(
            session,
            turns[0],
            "partial reply",
            xylo_core::MessageStatus::Failed,
        )
        .unwrap();
    // A request without a receipt must remain unknown rather than appear free.
    store
        .record_turn_usage(session, turns[0], Uuid::new_v4(), None)
        .unwrap();
    drop(store);
    let store = Store::open_existing(&path).unwrap();
    let failed_reply = Uuid::new_v5(&turns[0], b"assistant-interrupted");
    let other_reply = Uuid::new_v5(&turns[2], b"assistant-interrupted");
    let report = store
        .usage_report(session, vec![failed_reply, other_reply], 10, 20)
        .unwrap();
    assert_eq!(report.conversation.turns, 2);
    assert_eq!(report.conversation.input_tokens, 200);
    assert_eq!(report.conversation.output_tokens, 40);
    assert_eq!(report.conversation.cache_read_tokens, 60);
    assert_eq!(report.conversation.cache_write_tokens, 20);
    assert_eq!(report.conversation.unpriced_calls, 3);
    assert_eq!(report.daily.turns, 2);
    assert_eq!(report.daily.input_tokens, 200);
    assert_eq!(
        report.messages.len(),
        1,
        "Only this chat's failed reply is included"
    );
    assert_eq!(report.messages[&failed_reply].input_tokens, 100);

    store
        .complete_turn(
            session,
            turns[0],
            &serde_json::json!([]),
            "finished",
            &Usage {
                input_tokens: 100,
                output_tokens: 20,
                cache_read_tokens: Some(30),
                cache_write_tokens: Some(10),
                estimated_cost: Some(CostEstimate {
                    usd: 0.25,
                    unpriced_calls: 0,
                }),
                ..Default::default()
            },
        )
        .unwrap();
    let report = store
        .usage_report(
            session,
            vec![failed_reply, assistant_reply_id(turns[0])],
            0,
            i64::MAX,
        )
        .unwrap();
    assert_eq!(
        report.conversation.turns, 2,
        "Completion replaces pending receipts"
    );
    assert_eq!(report.conversation.input_tokens, 200);
    assert_eq!(report.conversation.estimated_usd, 0.25);
    assert_eq!(report.conversation.unpriced_calls, 1);
    assert!(!report.messages.contains_key(&failed_reply));
    assert_eq!(report.messages.len(), 1);
}
