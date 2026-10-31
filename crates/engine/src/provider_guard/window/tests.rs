use super::*;

fn window(context_tokens: u32, output_tokens: u32, served: Option<u32>) -> EffectiveWindow {
    EffectiveWindow::of(&ProviderConfig {
        context_tokens,
        output_tokens,
        served_context_tokens: served,
        ..Default::default()
    })
}

#[test]
fn effective_window_subtracts_output_and_margin_from_the_served_window() {
    let configured = window(32_768, 4096, None);
    assert_eq!(configured.context(), 32_768);
    assert_eq!(configured.tokens(), 32_768 - 4096 - 1024);
    assert!(!configured.small());
    // An Ollama server serving 4K while 32K is configured is budgeted at 4K.
    let served = window(32_768, 4096, Some(4096));
    assert_eq!(served.context(), 4096);
    assert_eq!(served.output(), 2048);
    assert_eq!(served.tokens(), 4096 - 2048 - 128);
    assert!(served.small());
}

#[test]
fn caps_scale_with_the_window_and_keep_the_old_limits_as_ceilings() {
    let small = window(8192, 2048, None);
    let large = window(1_000_000, 32_000, None);
    assert!(small.tool_result_bytes() <= 4 * 1024);
    assert_eq!(large.tool_result_bytes(), 25_000 * 3);
    assert!(small.tool_result_bytes() < large.tool_result_bytes());
    assert_eq!(small.file_read_lines(), 100);
    assert_eq!(large.file_read_lines(), 2000);
    assert!(small.file_read_bytes() < small.tool_result_bytes());
    assert!(large.subagent_result_bytes() <= 64 * 1024);
    assert!(small.subagent_result_bytes() < 4 * 1024);
    let (small_prompt, large_prompt) = (small.prompt_budget(), large.prompt_budget());
    assert!(large_prompt.instruction_bytes <= 24 * 1024);
    assert!(large_prompt.skills_index_bytes <= 8 * 1024);
    // The variable preamble stays a small share of a small window.
    let preamble_tokens =
        (small_prompt.instruction_bytes + small_prompt.skills_index_bytes) / BYTES_PER_TOKEN;
    assert!(preamble_tokens * 100 / small.tokens() as usize <= 20);
}
