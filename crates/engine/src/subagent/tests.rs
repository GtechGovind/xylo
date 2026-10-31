use super::*;

#[test]
fn child_answers_are_bounded_at_a_character_boundary() {
    let long = "界".repeat(30_000);
    let cut = bounded(&long);
    assert!(cut.len() <= MAX_RESULT_BYTES + 40);
    assert!(cut.ends_with("[answer truncated at 64 KB]"));
    assert_eq!(bounded("short"), "short");
}
