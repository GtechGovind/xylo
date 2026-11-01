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
    // An Ollama server serving 4K while 32K is configured is budgeted at 4K,
    // as a tiny window: a quarter of it is reserved for output.
    let served = window(32_768, 4096, Some(4096));
    assert_eq!(served.context(), 4096);
    assert_eq!(served.output(), 1024);
    assert_eq!(served.tokens(), 4096 - 1024 - 128);
    assert!(served.small() && served.tiny());
}

#[test]
fn only_windows_below_the_tiny_threshold_take_the_tiny_profile() {
    for (context, output, tiny) in [
        (2048, 2048, true),
        (4096, 512, true),
        (4096, 8192, true),
        (6144, 2048, true),
        (8192, 2048, false),
        (8192, 1024, false),
        (32_768, 4096, false),
    ] {
        let window = window(context, output, None);
        assert_eq!(window.tiny(), tiny, "{context}/{output}");
        assert!(window.output() <= output);
        if tiny {
            // The output reserve is a quarter of the window, at least the
            // floor, never more than half or than the configured limit.
            let reserve = (context / 4).max(TINY_OUTPUT_FLOOR).min(context / 2);
            assert_eq!(window.output(), output.min(reserve), "{context}/{output}");
            assert!(window.tokens() < TINY_WINDOW_TOKENS);
            assert!(window.prompt_budget().compact);
        } else {
            assert_eq!(window.output(), output.min(context / 2));
            assert!(!window.prompt_budget().compact);
        }
    }
}

#[test]
fn tiny_windows_size_results_and_thresholds_from_their_input_budget() {
    let tiny = window(4096, 2048, None);
    let small = window(8192, 2048, None);
    // A tool result is a share of W_eff, not the 1,024-token floor of
    // larger windows, and a file read leaves room for the result's fields.
    assert!(tiny.tool_result_bytes() < small.tool_result_bytes());
    assert!(tiny.tool_result_bytes() as u32 / BYTES_PER_TOKEN as u32 <= tiny.tokens() / 5);
    assert!(tiny.file_read_bytes() < tiny.tool_result_bytes());
    assert!(tiny.subagent_result_bytes() < small.subagent_result_bytes());
    assert!(tiny.recalled_context_bytes() < 1024);
    assert_eq!(small.recalled_context_bytes(), usize::MAX);
    // input_share places a threshold at a share of W_eff above the reserve.
    assert_eq!(tiny.input_share(0), tiny.context() - tiny.tokens());
    assert_eq!(tiny.input_share(100), tiny.context());
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
