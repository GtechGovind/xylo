//! Sidebar motion: which changes animate, and for how long.
use super::{Change, SidebarUi, conversation_changes};
use std::path::PathBuf;
use uuid::Uuid;
use xylo_core::{Conversation, TitleSource};

fn chat(id: Uuid, title: &str) -> Conversation {
    Conversation {
        id,
        title: title.into(),
        workspace: "/repo".into(),
        updated_at: 0,
        title_source: TitleSource::Generated,
        pinned: false,
        archived: false,
    }
}

#[test]
fn the_first_list_is_not_news() {
    let after = vec![chat(Uuid::new_v4(), "Fix the parser")];
    assert!(conversation_changes(&[], &after, false, Uuid::new_v4()).is_empty());
}

#[test]
fn new_chats_arrive_and_renamed_chats_retitle() {
    let (kept, renamed, new) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let before = vec![chat(kept, "Kept"), chat(renamed, "Old title")];
    let after = vec![
        chat(new, "Brand new"),
        chat(kept, "Kept"),
        chat(renamed, "New title"),
    ];
    let changes = conversation_changes(&before, &after, true, Uuid::new_v4());
    assert_eq!(
        changes,
        vec![Change::Arrived(new), Change::Retitled(renamed)]
    );
}

#[test]
fn the_open_chat_does_not_arrive_twice() {
    let session = Uuid::new_v4();
    let after = vec![chat(session, "Saved draft")];
    assert!(conversation_changes(&[], &after, true, session).is_empty());
}

#[test]
fn a_change_is_fresh_only_until_its_motion_settles() {
    let mut sidebar = SidebarUi::default();
    let project = Change::Expanded(PathBuf::from("/repo"));
    sidebar.note(project.clone());
    assert!(sidebar.fresh(&project));
    assert!(!sidebar.fresh(&Change::Arrived(Uuid::new_v4())));
    sidebar.forget_settled();
    assert!(sidebar.fresh(&project), "a fresh change survives pruning");
    sidebar.changes.insert(
        project.clone(),
        std::time::Instant::now() - crate::motion::SLOW,
    );
    assert!(!sidebar.fresh(&project));
    sidebar.forget_settled();
    assert!(sidebar.changes.is_empty());
}
