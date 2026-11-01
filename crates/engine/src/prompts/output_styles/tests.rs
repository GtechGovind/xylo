use super::*;
use xylo_runtime::workspace::output_styles::OutputStyle;

#[test]
fn output_styles_preserve_default_and_append_exact_custom_instructions() {
    let style = |key: &str, prompt: &str| OutputStyle {
        key: key.into(),
        label: key.into(),
        source: None,
        prompt: prompt.into(),
    };
    assert_eq!(apply("base".into(), &style("default", "")), "base");
    let explanatory = apply("base".into(), &style("explanatory", ""));
    assert!(explanatory.starts_with("base\n\n"));
    assert!(explanatory.contains("Explain"));
    let learning = apply("base".into(), &style("learning", ""));
    assert!(learning.contains("learning"));
    assert!(
        apply(
            "base".into(),
            &style("project:terse", "Reply in one sentence. {{literal}}")
        )
        .ends_with("Reply in one sentence. {{literal}}")
    );
}
