use super::{PaletteRow, palette_rows, title_matches};
use uuid::Uuid;
use xylo_core::{Conversation, TitleSource};

fn chat(title: &str, source: TitleSource) -> Conversation {
    Conversation {
        id: Uuid::new_v4(),
        title: title.into(),
        workspace: "/project".into(),
        updated_at: 0,
        title_source: source,
        pinned: false,
        archived: false,
    }
}

#[test]
fn titles_filter_instantly_ignoring_case_and_empty_lists_everything() {
    let chats = vec![
        chat("Build UI", TitleSource::User),
        chat("Fix tests", TitleSource::User),
        chat("rebuild cache", TitleSource::User),
    ];
    assert_eq!(title_matches(&chats, "  BUILD "), vec![0, 2]);
    assert_eq!(title_matches(&chats, ""), vec![0, 1, 2]);
    assert!(title_matches(&chats, "deploy").is_empty());
}

#[test]
fn greeting_titles_match_as_they_are_shown() {
    let chats = vec![chat("hello", TitleSource::Prompt)];
    assert_eq!(title_matches(&chats, "new chat"), vec![0]);
    assert!(title_matches(&chats, "hello").is_empty());
}

#[test]
fn messages_follow_chats_under_one_caption_and_status_comes_last() {
    assert_eq!(
        palette_rows(&[3, 5], 2, ""),
        vec![
            PaletteRow::Chat(3),
            PaletteRow::Chat(5),
            PaletteRow::MessagesHeading,
            PaletteRow::Message(0),
            PaletteRow::Message(1),
        ]
    );
    assert_eq!(
        palette_rows(&[], 0, "Searching messages…"),
        vec![PaletteRow::Status]
    );
    assert_eq!(palette_rows(&[1], 0, ""), vec![PaletteRow::Chat(1)]);
}
