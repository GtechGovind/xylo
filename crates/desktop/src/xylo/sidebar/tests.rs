use super::{SidebarRow, display_title, is_greeting, project_rows};
use std::{collections::BTreeSet, path::Path};
use uuid::Uuid;
use xylo_core::{Conversation, TitleSource};
fn chat(root: &str, title: &str) -> Conversation {
    Conversation {
        id: Uuid::new_v4(),
        title: title.into(),
        workspace: root.into(),
        updated_at: 0,
        title_source: TitleSource::Prompt,
        pinned: false,
        archived: false,
    }
}
#[test]
fn nests_chats_under_distinct_project_roots() {
    let chats = vec![
        chat("/a/app", "A"),
        chat("/b/app", "B"),
        chat("/a/app", "C"),
    ];
    assert_eq!(
        project_rows(&chats, None, &[], &BTreeSet::new(), false, false),
        vec![
            SidebarRow::Project("/a/app".into()),
            SidebarRow::Conversation(0),
            SidebarRow::Conversation(2),
            SidebarRow::Project("/b/app".into()),
            SidebarRow::Conversation(1)
        ]
    );
}
#[test]
fn collapse_keeps_empty_open_projects_and_pagination() {
    assert_eq!(
        project_rows(
            &[chat("/a", "A")],
            Some(Path::new("/b")),
            &["/c".into()],
            &BTreeSet::from(["/a".into()]),
            false,
            true
        ),
        vec![
            SidebarRow::Project("/a".into()),
            SidebarRow::Project("/b".into()),
            SidebarRow::Project("/c".into()),
            SidebarRow::More
        ]
    );
}
#[test]
fn unsaved_chat_is_nested_and_collapses_with_its_project() {
    let root = Path::new("/project");
    assert_eq!(
        project_rows(&[], Some(root), &[], &BTreeSet::new(), true, false),
        vec![SidebarRow::Project(root.into()), SidebarRow::DraftChat]
    );
    assert_eq!(
        project_rows(
            &[],
            Some(root),
            &[],
            &BTreeSet::from([root.into()]),
            true,
            false
        ),
        vec![SidebarRow::Project(root.into())]
    );
}
#[test]
fn greeting_prompts_read_as_new_chat_until_a_title_arrives() {
    for greeting in ["hi", "Hello!", "hey there", "Good morning, Xylo", "yo 👋"] {
        assert!(is_greeting(greeting), "{greeting}");
    }
    for topic in [
        "hi, fix the build",
        "good tests",
        "hello world program",
        "there",
    ] {
        assert!(!is_greeting(topic), "{topic}");
    }
    assert_eq!(display_title(&chat("/a", "hey")), "New chat");
    assert_eq!(display_title(&chat("/a", "  ")), "New chat");
    assert_eq!(display_title(&chat("/a", "Fix tests")), "Fix tests");
    let mut named = chat("/a", "hey");
    named.title_source = TitleSource::User;
    assert_eq!(display_title(&named), "hey");
}
#[test]
fn pinned_chats_lead_their_project_and_keep_list_order() {
    let mut chats = vec![
        chat("/a", "A"),
        chat("/a", "B"),
        chat("/b", "C"),
        chat("/a", "D"),
        chat("/b", "E"),
    ];
    chats[3].pinned = true;
    chats[4].pinned = true;
    assert_eq!(
        project_rows(&chats, None, &[], &BTreeSet::new(), false, false),
        vec![
            SidebarRow::Project("/a".into()),
            SidebarRow::Conversation(3),
            SidebarRow::Conversation(0),
            SidebarRow::Conversation(1),
            SidebarRow::Project("/b".into()),
            SidebarRow::Conversation(4),
            SidebarRow::Conversation(2),
        ]
    );
}
