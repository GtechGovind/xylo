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
//!
//! A window whose input budget under the usual output reserve is below
//! [`TINY_WINDOW_TOKENS`] (every 4K or 6K window; an 8K window only with an
//! output limit above 2,304 tokens) is *tiny*: the output reserve drops to a quarter of the window (at least
//! [`TINY_OUTPUT_FLOOR`]; continuations cover longer answers), the system
//! prompt and the resident tools take their compact forms, and the
//! compaction thresholds are shares of `W_eff` rather than of the window, so
//! the larger fixed share of a tiny window does not leave masking and
//! summaries without room to work.

use crate::prompts::PromptBudget;
use xylo_core::ProviderConfig;

/// Windows whose `W_eff` is below this are small: fewer tools stay resident
/// and the summary triggers earlier.
pub(crate) const SMALL_WINDOW_TOKENS: u32 = 16_384;
/// Windows whose `W_eff` under the usual output reserve is below this are
/// tiny (see the module documentation). An 8K window with the default 2K
/// output limit (`W_eff` 5,888) stays just above it, on the small profile.
pub(crate) const TINY_WINDOW_TOKENS: u32 = 5_632;
/// The smallest output reserve of a tiny window.
pub(crate) const TINY_OUTPUT_FLOOR: u32 = 512;
/// Room a tiny window's `file_read` leaves in the tool-result budget for the
/// result's own fields and JSON escaping.
const TINY_READ_ENVELOPE_BYTES: usize = 384;
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
    /// The tiny-window profile applies.
    tiny: bool,
}

impl EffectiveWindow {
    /// The window of `config`.
    pub(crate) fn of(config: &ProviderConfig) -> Self {
        let context = config.context_window();
        // A served window smaller than the configured output limit must still
        // leave room for input.
        let output = config.output_tokens.min(context / 2);
        let margin = (context / 32).clamp(128, 4096);
        let input = |output: u32| context.saturating_sub(output).saturating_sub(margin);
        let tiny = input(output) < TINY_WINDOW_TOKENS;
        // A tiny window reserves less for output: a longer answer is
        // continued (the loop recovers from the Length finish reason).
        let output = if tiny {
            output.min((context / 4).max(TINY_OUTPUT_FLOOR))
        } else {
            output
        };
        Self {
            context,
            output,
            tokens: input(output),
            tiny,
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
        self.tiny || self.tokens < SMALL_WINDOW_TOKENS
    }

    /// Whether the tiny-window profile applies: compact system prompt and
    /// tools, the minimal resident tool set and thresholds scaled from
    /// `W_eff`.
    pub(crate) fn tiny(&self) -> bool {
        self.tiny
    }

    /// The window an input share of `percent` of `W_eff` corresponds to: the
    /// output reserve and the safety margin plus that share. Compaction
    /// thresholds of tiny windows are placed here, because their output
    /// reserve and margin are a large share of the window.
    pub(crate) fn input_share(&self, percent: u32) -> u32 {
        let share = (u64::from(self.tokens) * u64::from(percent) / 100) as u32;
        self.context - self.tokens + share
    }

    /// `percent` of `W_eff` in tokens, clamped, as bytes.
    fn share(&self, percent: u32, min_tokens: u32, max_tokens: u32) -> usize {
        let tokens = (u64::from(self.tokens) * u64::from(percent) / 100) as u32;
        tokens.clamp(min_tokens, max_tokens) as usize * BYTES_PER_TOKEN
    }

    /// Largest tool result the model sees inline: about 12 % of `W_eff`, at
    /// least 1,024 tokens and at most 25,000; larger results are shortened
    /// and saved in full to a file the model can page.
    /// A tiny window gives a result about 16 % of `W_eff`, at most 1,024
    /// tokens, so a tool round leaves room for the next.
    pub(crate) fn tool_result_bytes(&self) -> usize {
        if self.tiny {
            self.share(16, 256, 1024)
        } else {
            self.share(12, 1024, 25_000)
        }
    }

    /// Default line window of one `file_read`: one line per 64 tokens of
    /// `W_eff`, between 100 and 2,000 lines.
    pub(crate) fn file_read_lines(&self) -> u64 {
        u64::from(self.tokens / 64).clamp(100, 2000)
    }

    /// Bytes of file text in one `file_read`, leaving room in the tool-result
    /// budget for the result's own fields.
    pub(crate) fn file_read_bytes(&self) -> usize {
        if self.tiny {
            self.tool_result_bytes()
                .saturating_sub(TINY_READ_ENVELOPE_BYTES)
                .max(512)
        } else {
            self.tool_result_bytes().saturating_sub(1024).max(2048)
        }
    }

    /// Largest helper-agent answer returned to the parent: about 6 % of
    /// `W_eff`, between 768 and 8,192 tokens; about 10 % and at most 768
    /// tokens in a tiny window.
    pub(crate) fn subagent_result_bytes(&self) -> usize {
        if self.tiny {
            self.share(10, 192, 768)
        } else {
            self.share(6, 768, 8192)
        }
    }

    /// Largest recalled-context note a turn appends: unbounded, except in a
    /// tiny window, where a note over about 6 % of `W_eff` is replaced by a
    /// one-line pointer to `memory_search`.
    pub(crate) fn recalled_context_bytes(&self) -> usize {
        if self.tiny {
            self.share(6, 0, u32::MAX)
        } else {
            usize::MAX
        }
    }

    /// Budgets of the variable system-prompt sections: project instructions
    /// about 8 % of `W_eff` and the skills index about 4 %, never more than
    /// [`PromptBudget::LARGEST`], so the fixed preamble stays a small share of
    /// a small window. A tiny window gets the compact prompt, without the
    /// floors: a section that does not fit whole is left out (and the prompt
    /// says so) rather than truncated.
    pub(crate) fn prompt_budget(&self) -> PromptBudget {
        let largest = PromptBudget::LARGEST;
        let floor = |tokens: u32| if self.tiny { 0 } else { tokens };
        PromptBudget {
            instruction_bytes: self
                .share(8, floor(512), u32::MAX)
                .min(largest.instruction_bytes),
            skills_index_bytes: self
                .share(4, floor(384), u32::MAX)
                .min(largest.skills_index_bytes),
            compact: self.tiny,
        }
    }
}

#[cfg(test)]
mod tests;
