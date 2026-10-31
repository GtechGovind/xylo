use super::{STATUS_TTL, StatusLine, ToastUndo, UNDO_TTL, product_error};

#[test]
fn product_error_keeps_the_producers_words() {
    assert_eq!(
        product_error("\n  The worker exited\nstack trace"),
        "The worker exited"
    );
    assert_eq!(
        product_error("HTTP 401 from an unrelated helper"),
        "HTTP 401 from an unrelated helper"
    );
    assert_eq!(product_error(""), "Something went wrong. Please try again.");
}

#[test]
fn a_new_status_is_visible_and_an_empty_one_is_not() {
    let fresh: StatusLine = "Ready".into();
    assert!(fresh.visible());
    let empty: StatusLine = String::new().into();
    assert!(!empty.visible());
    let stale = StatusLine {
        text: "Old".into(),
        posted: std::time::Instant::now() - STATUS_TTL,
        undo: None,
    };
    assert!(!stale.visible());
}

#[test]
fn a_notice_with_undo_stays_long_enough_to_reach_it() {
    let chat = xylo_core::Conversation {
        id: uuid::Uuid::new_v4(),
        title: "Old chat".into(),
        workspace: "/project".into(),
        updated_at: 0,
        title_source: Default::default(),
        pinned: false,
        archived: true,
    };
    let mut undo = StatusLine::with_undo("Chat archived", ToastUndo::Unarchive(chat));
    undo.posted = std::time::Instant::now() - STATUS_TTL;
    assert!(undo.visible());
    undo.posted = std::time::Instant::now() - UNDO_TTL;
    assert!(!undo.visible());
}
