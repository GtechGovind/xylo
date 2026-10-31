use super::{UNTRUSTED_TAG, memory_title, origin_text, scope_label};
use xylo_core::MemoryOrigin;

#[test]
fn memory_titles_use_the_first_line_and_stay_short() {
    assert_eq!(memory_title("  Use tabs\nnot spaces"), "Use tabs");
    assert_eq!(memory_title(""), "Untitled memory");
    let long = "x".repeat(100);
    assert_eq!(memory_title(&long).chars().count(), 81);
}

#[test]
fn scope_tags_name_the_project_folder() {
    assert_eq!(scope_label("global"), "All projects");
    assert_eq!(scope_label("/Users/me/work/app"), "app");
}

#[test]
fn origins_read_in_the_persons_words_and_flag_outside_content() {
    assert_eq!(origin_text(MemoryOrigin::UserTyped), "You added this");
    assert_eq!(origin_text(MemoryOrigin::UserApproved), "You approved this");
    assert_eq!(
        origin_text(MemoryOrigin::AgentInferred),
        "Xylo inferred this"
    );
    assert_eq!(
        origin_text(MemoryOrigin::ToolOutput),
        "From a tool's output"
    );
    assert_eq!(origin_text(MemoryOrigin::WebContent), "From web content");
    // Exactly the origins never recalled automatically carry the warning tag.
    let flagged: Vec<_> = [
        MemoryOrigin::UserTyped,
        MemoryOrigin::UserApproved,
        MemoryOrigin::AgentInferred,
        MemoryOrigin::ToolOutput,
        MemoryOrigin::WebContent,
    ]
    .into_iter()
    .filter(|origin| origin.untrusted())
    .collect();
    assert_eq!(
        flagged,
        [MemoryOrigin::ToolOutput, MemoryOrigin::WebContent]
    );
    assert_eq!(UNTRUSTED_TAG, "Not used automatically");
}
