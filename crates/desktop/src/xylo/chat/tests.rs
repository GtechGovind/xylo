use super::{is_image_path, reply_status_label, speakable_text};
use std::path::Path;

#[test]
fn read_aloud_speaks_prose_not_markup_or_code() {
    let answer = "## Findings\n\nThe **runtime host** routes `WorkerCommand`s. See [the docs](https://example.invalid/docs).\n\n```rust\nfn main() {}\n```\n\n| a | b |\n|---|---|\n- First point\n";
    assert_eq!(
        speakable_text(answer),
        "Findings\nThe runtime host routes WorkerCommands. See the docs.\na  b\nFirst point"
    );
}

#[test]
fn only_image_files_attach_as_images() {
    assert!(is_image_path(Path::new("/tmp/shot.PNG")));
    assert!(is_image_path(Path::new("photo.jpeg")));
    assert!(!is_image_path(Path::new("notes.md")));
    assert!(!is_image_path(Path::new("Makefile")));
}

#[test]
fn a_reloaded_cut_off_answer_says_it_reached_the_output_limit() {
    use xylo_core::MessageStatus;
    assert_eq!(reply_status_label(MessageStatus::Complete), None);
    let label = reply_status_label(MessageStatus::Truncated).unwrap();
    assert!(label.contains("length limit"), "{label}");
}
