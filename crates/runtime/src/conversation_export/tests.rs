use super::*;
#[test]
fn exports_all_pages_in_order_to_adjacent_files() {
    let root = tempfile::tempdir().unwrap();
    let store = Store::open(root.path().join("database")).unwrap();
    let session = Uuid::new_v4();
    store
        .upsert_conversation(&xylo_core::Conversation {
            id: session,
            title: "Large chat".into(),
            workspace: root.path().into(),
            updated_at: 1,
            title_source: Default::default(),
            pinned: false,
            archived: false,
        })
        .unwrap();
    for index in 0..405 {
        store
            .append_message(&ChatMessage {
                activity_count: 0,
                activity_summary: None,
                turn_id: None,
                id: Uuid::new_v4(),
                session_id: session,
                role: "user".into(),
                content: format!("message {index}\nwith newline"),
                created_at: index,
                attachments: Vec::new(),
                reasoning: None,
                status: Default::default(),
            })
            .unwrap();
    }
    let path = root.path().join("chat.md");
    export(&store, session, &path).unwrap();
    let jsonl = std::fs::read_to_string(path.with_extension("jsonl")).unwrap();
    let records = jsonl
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(records.len(), 405);
    assert_eq!(records[0]["message"]["content"], "message 0\nwith newline");
    assert_eq!(
        records[404]["message"]["content"],
        "message 404\nwith newline"
    );
    assert!(
        std::fs::read_to_string(path)
            .unwrap()
            .contains("message 404")
    );
}
