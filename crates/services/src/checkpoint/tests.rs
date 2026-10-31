use super::*;
use serde_json::json;
use xylo_core::Conversation;

fn store_with_session() -> (tempfile::TempDir, Store, Uuid) {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("xylo.db")).unwrap();
    let session = Uuid::new_v4();
    store
        .upsert_conversation(&Conversation {
            id: session,
            title: "Checkpoint fixture".into(),
            workspace: directory.path().into(),
            updated_at: now_ms(),
            title_source: Default::default(),
            pinned: false,
            archived: false,
        })
        .unwrap();
    (directory, store, session)
}

fn rows(texts: &[&str]) -> Arc<[String]> {
    texts
        .iter()
        .map(|text| json!({"role":"user","content":text}).to_string())
        .collect()
}

fn contents(store: &Store, session: Uuid) -> Vec<String> {
    store
        .checkpoint(session)
        .unwrap()
        .unwrap()
        .messages
        .as_array()
        .unwrap()
        .iter()
        .map(|message| message["content"].as_str().unwrap().to_owned())
        .collect()
}

/// Overwrites one stored row behind the store's back.
fn tamper(store: &Store, session: Uuid, position: i64) {
    store
        .call(move |conn| {
            conn.execute(
                "UPDATE checkpoint_messages SET json=?1 WHERE session_id=?2 AND position=?3",
                params![
                    json!({"role":"user","content":"tampered"}).to_string(),
                    session.to_string(),
                    position
                ],
            )?;
            Ok(())
        })
        .unwrap();
}

#[test]
fn a_current_base_writes_only_the_changed_suffix() {
    let (_directory, store, session) = store_with_session();
    let turn = Uuid::new_v4();
    let first = store
        .save_checkpoint_messages(session, turn, rows(&["a", "b", "c"]), None)
        .unwrap();
    tamper(&store, session, 0);
    let second = store
        .save_checkpoint_messages(
            session,
            turn,
            rows(&["a", "b", "c", "d"]),
            Some(CheckpointBase {
                revision: first,
                unchanged: 3,
            }),
        )
        .unwrap();
    assert_eq!(second, first + 1);
    // Row 0 was not rewritten: only position 3 was.
    assert_eq!(contents(&store, session), ["tampered", "b", "c", "d"]);
}

#[test]
fn without_a_current_base_the_stored_rows_decide_what_changed() {
    let (_directory, store, session) = store_with_session();
    let turn = Uuid::new_v4();
    let first = store
        .save_checkpoint_messages(session, turn, rows(&["a", "b", "c"]), None)
        .unwrap();
    // Another writer (guidance, a completed turn) wrote in between.
    store
        .save_checkpoint(
            session,
            turn,
            &json!([{"role":"user","content":"a"},{"role":"user","content":"guidance"}]),
        )
        .unwrap();
    tamper(&store, session, 0);
    store
        .save_checkpoint_messages(
            session,
            turn,
            rows(&["a", "b", "c"]),
            Some(CheckpointBase {
                revision: first,
                unchanged: 3,
            }),
        )
        .unwrap();
    assert_eq!(contents(&store, session), ["a", "b", "c"]);
}

#[test]
fn a_shorter_rewrite_replaces_the_tail() {
    let (_directory, store, session) = store_with_session();
    let turn = Uuid::new_v4();
    let first = store
        .save_checkpoint_messages(session, turn, rows(&["a", "b", "c", "d"]), None)
        .unwrap();
    store
        .save_checkpoint_messages(
            session,
            turn,
            rows(&["summary", "d"]),
            Some(CheckpointBase {
                revision: first,
                unchanged: 0,
            }),
        )
        .unwrap();
    assert_eq!(contents(&store, session), ["summary", "d"]);
    let next_turn = Uuid::new_v4();
    store
        .save_checkpoint(session, next_turn, &json!([]))
        .unwrap();
    let checkpoint = store.checkpoint(session).unwrap().unwrap();
    assert_eq!(checkpoint.turn_id, next_turn);
    assert_eq!(checkpoint.messages, json!([]));
}

#[test]
fn missing_rows_fail_closed() {
    let (_directory, store, session) = store_with_session();
    store
        .save_checkpoint_messages(session, Uuid::new_v4(), rows(&["a", "b"]), None)
        .unwrap();
    store
        .call(move |conn| {
            conn.execute(
                "DELETE FROM checkpoint_messages WHERE session_id=?1 AND position=1",
                [session.to_string()],
            )?;
            Ok(())
        })
        .unwrap();
    assert!(store.checkpoint(session).is_err());
    assert!(store.checkpoint(Uuid::new_v4()).unwrap().is_none());
}
