use super::{memory_title, scope_label};

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
