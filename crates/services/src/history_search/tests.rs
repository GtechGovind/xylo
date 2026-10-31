use super::*;
use xylo_core::now_ms;
fn setup() -> (tempfile::TempDir, Store, Uuid) {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("history.db")).unwrap();
    let id = Uuid::new_v4();
    store
        .upsert_conversation(&Conversation {
            id,
            title: "Original title".into(),
            workspace: directory.path().into(),
            updated_at: 7,
            title_source: Default::default(),
            pinned: false,
            archived: false,
        })
        .unwrap();
    (directory, store, id)
}
fn message(store: &Store, session: Uuid, text: &str) -> Uuid {
    let id = Uuid::new_v4();
    store
        .append_message(&ChatMessage {
            attachments: Vec::new(),
            id,
            session_id: session,
            role: "user".into(),
            content: text.into(),
            created_at: 7,
            status: Default::default(),
        })
        .unwrap();
    id
}
#[test]
fn triggers_search_all_history_with_source_ids_and_bounded_snippets() {
    let (_directory, store, session) = setup();
    let first = message(
        &store,
        session,
        "Older needlebody outside the visible sidebar.",
    );
    let found = store.search_history("needlebody", None, 20).unwrap();
    assert_eq!(found.items[0].message_id, Some(first));
    assert!(found.items[0].snippet.contains("needlebody"));
    let second = message(
        &store,
        session,
        "New needlebody captured by an insert trigger.",
    );
    let found = store.search_history("needlebody", None, 20).unwrap();
    assert_eq!(found.items.len(), 1, "one hit per conversation");
    assert!(found.items[0].message_id == Some(first) || found.items[0].message_id == Some(second));
    store
        .call(move |conn| {
            conn.execute(
                "UPDATE messages SET json=json_set(json,'$.content','replacementword') WHERE id=?1",
                [second.to_string()],
            )?;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        store
            .search_history("replacementword", None, 20)
            .unwrap()
            .items[0]
            .message_id,
        Some(second)
    );
    store
        .call(move |conn| {
            conn.execute("DELETE FROM messages WHERE id=?1", [second.to_string()])?;
            Ok(())
        })
        .unwrap();
    assert!(
        store
            .search_history("replacementword", None, 20)
            .unwrap()
            .items
            .is_empty()
    );
    store
        .upsert_conversation(&Conversation {
            id: session,
            title: "Renamedtitle".into(),
            workspace: "/fixture".into(),
            updated_at: now_ms(),
            title_source: Default::default(),
            pinned: false,
            archived: false,
        })
        .unwrap();
    let titles = store.search_history("Renamedtitle", None, 20).unwrap();
    assert_eq!(titles.items[0].message_id, None);
    assert!(
        store
            .search_history("Original", None, 20)
            .unwrap()
            .items
            .is_empty()
    );
}
#[test]
fn tied_keyset_pages_return_each_conversation_once_and_escape_fts_operators() {
    let (_directory, store, _session) = setup();
    for id in 1..=5 {
        let session = Uuid::from_u128(id);
        store
            .upsert_conversation(&Conversation {
                id: session,
                title: "Searchable sharedtitle".into(),
                workspace: "/fixture".into(),
                updated_at: 9,
                title_source: Default::default(),
                pinned: false,
                archived: false,
            })
            .unwrap();
        message(&store, session, "Sharedtitle repeated first");
        message(&store, session, "Sharedtitle repeated second");
    }
    let mut cursor = None;
    let mut ids = Vec::new();
    loop {
        let page = store.search_history("sharedtitle", cursor, 2).unwrap();
        ids.extend(page.items.iter().map(|item| item.conversation.id));
        if !page.has_more {
            break;
        }
        cursor = page.before;
    }
    assert_eq!(
        ids,
        vec![5, 4, 3, 2, 1]
            .into_iter()
            .map(Uuid::from_u128)
            .collect::<Vec<_>>()
    );
    assert!(store.search_history("\" OR * NOT () : ^", None, 10).is_ok());
    assert!(store.search_history(&"x".repeat(513), None, 10).is_err());
}
#[test]
fn message_jump_and_later_pages_preserve_append_order_without_gaps_or_cross_session_anchors() {
    let (_directory, store, session) = setup();
    let mut ids = Vec::new();
    for index in 0..10 {
        ids.push(message(&store, session, &index.to_string()));
    }
    let window = store.history_window(session, ids[4], 5).unwrap();
    assert_eq!(
        window
            .messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        ["2", "3", "4", "5", "6"]
    );
    assert!(window.has_older && window.has_newer);
    assert_eq!(window.anchor, Some(ids[4]));
    let later = store
        .history_after(session, window.after.unwrap(), 2)
        .unwrap();
    assert!(later.append);
    assert_eq!(
        later
            .messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        ["7", "8"]
    );
    assert!(later.has_newer);
    let last = store
        .history_after(session, later.after.unwrap(), 200)
        .unwrap();
    assert_eq!(last.messages[0].content, "9");
    assert!(!last.has_newer);
    assert!(store.history_window(Uuid::new_v4(), ids[4], 5).is_err());
    assert_eq!(
        store
            .history_window(session, ids[0], 1)
            .unwrap()
            .messages
            .len(),
        1
    );
    let id = message(&store, session, "new arrival");
    let arrival = store
        .history_after(session, last.after.unwrap(), 10)
        .unwrap();
    assert_eq!(arrival.messages[0].id, id);
}
#[test]
fn best_hit_is_the_best_ranked_document_not_the_newest() {
    let (_directory, store, session) = setup();
    let focused = message(&store, session, "rankneedle rankneedle rankneedle");
    message(
        &store,
        session,
        "A much longer and newer message that mentions rankneedle once among many other unrelated words about builds, tests, folders and settings.",
    );
    let found = store.search_history("rankneedle", None, 20).unwrap();
    assert_eq!(found.items.len(), 1);
    assert_eq!(found.items[0].message_id, Some(focused));
}
#[test]
fn writes_after_reopen_remain_searchable() {
    let (directory, store, session) = setup();
    message(&store, session, "Persistentneedle");
    drop(store);
    let store = Store::open(directory.path().join("history.db")).unwrap();
    message(&store, session, "Freshneedle after reopen");
    for needle in ["Persistentneedle", "Freshneedle"] {
        assert_eq!(
            store.search_history(needle, None, 10).unwrap().items.len(),
            1
        );
    }
}
