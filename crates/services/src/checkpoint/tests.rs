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

#[test]
fn fallback_metadata_survives_same_turn_rewrites_but_cannot_escape_its_turn() {
    let (_directory, store, session) = store_with_session();
    let turn = Uuid::new_v4();
    store
        .save_checkpoint(session, turn, &json!([{"role":"user","content":"Work"}]))
        .unwrap();
    store
        .select_checkpoint_fallback(session, turn, "alternative")
        .unwrap();
    store
        .save_checkpoint(
            session,
            turn,
            &json!([{"role":"user","content":"Condensed"}]),
        )
        .unwrap();
    assert_eq!(
        store.checkpoint_fallback(session, turn).unwrap().as_deref(),
        Some("alternative")
    );
    assert_eq!(
        store
            .checkpoint(session)
            .unwrap()
            .unwrap()
            .fallback_model
            .as_deref(),
        Some("alternative")
    );
    let next = Uuid::new_v4();
    assert!(store.checkpoint_fallback(session, next).unwrap().is_none());
    assert!(
        store
            .select_checkpoint_fallback(session, next, "wrong")
            .is_err()
    );
    store
        .save_checkpoint(
            session,
            next,
            &json!([{"role":"user","content":"Fresh turn"}]),
        )
        .unwrap();
    assert!(store.checkpoint_fallback(session, next).unwrap().is_none());
    assert!(store.checkpoint_fallback(session, turn).unwrap().is_none());
    assert!(
        store
            .select_checkpoint_fallback(session, turn, "stale")
            .is_err()
    );
    assert!(
        store
            .checkpoint(session)
            .unwrap()
            .unwrap()
            .fallback_model
            .is_none()
    );
}

#[test]
fn a_database_missing_fallback_metadata_is_rejected_or_reset_without_migration() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("xylo.db");
    {
        let old = rusqlite::Connection::open(&path).unwrap();
        old.execute_batch(&crate::schema::SCHEMA.replace(",fallback_model TEXT", ""))
            .unwrap();
        old.pragma_update(None, "user_version", crate::schema::VERSION)
            .unwrap();
        old.execute("INSERT INTO conversations VALUES('old','{}',0)", [])
            .unwrap();
    }
    assert!(
        Store::open_existing(&path).is_err(),
        "Workers cannot accept an old checkpoint schema even at the current version"
    );
    let old = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        old.query_row("SELECT COUNT(*) FROM conversations", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    drop(old);
    let fresh = Store::open(&path).unwrap();
    assert!(fresh.reset_notice().unwrap().reason.contains("tables"));
    assert!(fresh.conversations(10).unwrap().is_empty());
    assert!(
        fresh
            .checkpoint_fallback(Uuid::new_v4(), Uuid::new_v4())
            .unwrap()
            .is_none()
    );
}
