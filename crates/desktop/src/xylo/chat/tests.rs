use super::{is_image_path, speakable_text, time_label};
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
fn answer_times_read_as_a_clock_today_and_a_date_before() {
    use chrono::{Local, TimeZone};
    let now = Local.with_ymd_and_hms(2026, 10, 31, 15, 8, 0).unwrap();
    let today = Local.with_ymd_and_hms(2026, 10, 31, 3, 8, 0).unwrap();
    let earlier = Local.with_ymd_and_hms(2026, 10, 30, 21, 5, 0).unwrap();
    let last_year = Local.with_ymd_and_hms(2025, 1, 2, 9, 0, 0).unwrap();
    let ms = |at: chrono::DateTime<Local>| at.timestamp_millis();
    assert_eq!(time_label(ms(today), ms(now)), "3:08 AM");
    assert_eq!(time_label(ms(earlier), ms(now)), "Oct 30, 9:05 PM");
    assert_eq!(time_label(ms(last_year), ms(now)), "Jan 2 2025, 9:00 AM");
}

#[test]
fn only_image_files_attach_as_images() {
    assert!(is_image_path(Path::new("/tmp/shot.PNG")));
    assert!(is_image_path(Path::new("photo.jpeg")));
    assert!(!is_image_path(Path::new("notes.md")));
    assert!(!is_image_path(Path::new("Makefile")));
}
