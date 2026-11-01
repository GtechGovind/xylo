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
