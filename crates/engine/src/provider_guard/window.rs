//! The effective context window and every size derived from it.
//!
//! `W_eff` is the number of input tokens a request may fill: the provider's
//! context window ([`ProviderConfig::context_window`], the configured window
//! lowered to what a local server reports serving) minus the output reserve
//! and a safety margin for chat-template tokens the estimate cannot see.
//! Reasoning needs no reserve of its own: every supported API counts
//! reasoning tokens against the requested output limit, which the output
//! reserve already covers.
//!
//! Every size that reaches the model scales from `W_eff`, so no single tool
//! result, file read, helper answer or the fixed preamble can fill a small
//! window, while large windows keep the previous fixed caps as upper bounds.
//! All sizes are pure functions of the provider configuration and the served
//! context, which is probed once per endpoint and model per process: two
//! turns of a session see the same caps, which keeps tool presentations and
//! the system prompt byte-stable across turns.

use crate::prompts::PromptBudget;
use xylo_core::ProviderConfig;

/// Windows whose `W_eff` is below this are small: fewer tools stay resident
/// and the summary triggers earlier.
pub(crate) const SMALL_WINDOW_TOKENS: u32 = 16_384;
/// Byte caps are derived from token budgets at this conservative ratio;
/// code and JSON average more bytes per token, so a cap rarely overshoots.
const BYTES_PER_TOKEN: usize = 3;

/// A model's effective input window and the budgets scaled from it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct EffectiveWindow {
    /// The window a request must fit.
    context: u32,
    /// Output tokens reserved in every request.
    output: u32,
    /// `W_eff`: input tokens available to a request.
    tokens: u32,
}

impl EffectiveWindow {
    /// The window of `config`.
    pub(crate) fn of(config: &ProviderConfig) -> Self {
        let context = config.context_window();
        // A served window smaller than the configured output limit must still
        // leave room for input.
        let output = config.output_tokens.min(context / 2);
        let margin = (context / 32).clamp(128, 4096);
        Self {
            context,
            output,
            tokens: context.saturating_sub(output).saturating_sub(margin),
        }
    }

    /// The context window requests are fitted to.
    pub(crate) fn context(&self) -> u32 {
        self.context
    }

    /// The output limit every request may use.
    pub(crate) fn output(&self) -> u32 {
        self.output
    }

    /// `W_eff`: input tokens available to one request.
    pub(crate) fn tokens(&self) -> u32 {
        self.tokens
    }

    /// Whether the window is small enough that the resident tool set shrinks
    /// and summaries trigger earlier.
    pub(crate) fn small(&self) -> bool {
        self.tokens < SMALL_WINDOW_TOKENS
    }

    /// `percent` of `W_eff` in tokens, clamped, as bytes.
    fn share(&self, percent: u32, min_tokens: u32, max_tokens: u32) -> usize {
        let tokens = (u64::from(self.tokens) * u64::from(percent) / 100) as u32;
        tokens.clamp(min_tokens, max_tokens) as usize * BYTES_PER_TOKEN
    }

    /// Largest tool result the model sees inline: about 12 % of `W_eff`, at
    /// least 1,024 tokens and at most 25,000; larger results are shortened
    /// and saved in full to a file the model can page.
    pub(crate) fn tool_result_bytes(&self) -> usize {
        self.share(12, 1024, 25_000)
    }

    /// Default line window of one `file_read`: one line per 64 tokens of
    /// `W_eff`, between 100 and 2,000 lines.
    pub(crate) fn file_read_lines(&self) -> u64 {
        u64::from(self.tokens / 64).clamp(100, 2000)
    }

    /// Bytes of file text in one `file_read`, leaving room in the tool-result
    /// budget for the result's own fields.
    pub(crate) fn file_read_bytes(&self) -> usize {
        self.tool_result_bytes().saturating_sub(1024).max(2048)
    }

    /// Largest helper-agent answer returned to the parent: about 6 % of
    /// `W_eff`, between 768 and 8,192 tokens.
    pub(crate) fn subagent_result_bytes(&self) -> usize {
        self.share(6, 768, 8192)
    }

    /// Budgets of the variable system-prompt sections: project instructions
    /// about 8 % of `W_eff` and the skills index about 4 %, never more than
    /// [`PromptBudget::LARGEST`], so the fixed preamble stays a small share of
    /// a small window.
    pub(crate) fn prompt_budget(&self) -> PromptBudget {
        let largest = PromptBudget::LARGEST;
        PromptBudget {
            instruction_bytes: self.share(8, 512, u32::MAX).min(largest.instruction_bytes),
            skills_index_bytes: self.share(4, 384, u32::MAX).min(largest.skills_index_bytes),
        }
    }
}

#[cfg(test)]
mod tests;
