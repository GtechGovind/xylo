use super::*;
use xylo_core::ProviderConfig;

fn window(context_tokens: u32) -> EffectiveWindow {
    EffectiveWindow::of(&ProviderConfig {
        context_tokens,
        output_tokens: 2048,
        ..Default::default()
    })
}

#[test]
fn child_answers_are_bounded_at_a_character_boundary() {
    let large = window(200_000);
    let limit = large.subagent_result_bytes();
    let long = "界".repeat(30_000);
    let cut = bounded(&long, large);
    assert!(cut.len() <= limit + 48);
    assert!(cut.ends_with(&format!("[answer truncated at {limit} bytes]")));
    assert_eq!(bounded("short", large), "short");
}

#[test]
fn a_small_window_gets_a_shorter_child_answer() {
    let long = "word ".repeat(20_000);
    let small = bounded(&long, window(8192));
    let large = bounded(&long, window(200_000));
    assert!(small.len() < large.len());
    assert!(small.len() <= window(8192).subagent_result_bytes() + 48);
}
