use super::TextShape;

#[test]
fn only_empty_and_blank_transitions_change_what_the_workspace_shows() {
    assert_eq!(TextShape::of("hello"), TextShape::of("hello world"));
    assert_eq!(
        TextShape::of(""),
        TextShape {
            empty: true,
            blank: true
        }
    );
    assert_ne!(TextShape::of(""), TextShape::of("h"));
    assert_ne!(TextShape::of(" "), TextShape::of(" h"));
    assert_ne!(TextShape::of(""), TextShape::of(" "));
}
