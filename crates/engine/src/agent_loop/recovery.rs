//! What the loop does with a model reply that has no tool calls.
//!
//! The reply's finish reason decides, never the model's own judgement:
//! - A reply the output limit cut off is continued: the partial reply stays
//!   in the conversation, an application note asks the model to carry on,
//!   and the parts join into one answer. At most [`MAX_CONTINUATIONS`]
//!   continuations follow each other; after that the answer is returned as
//!   it stands.
//! - A reply with no answer text (including one whose reasoning used the
//!   whole output limit) or with a tool call the provider could not parse is
//!   asked for once more with a note. A second unusable reply in a row fails
//!   the turn with [`ModelError::EmptyResponse`].
//! - Anything else is the answer.
//!
//! The state covers one run of consecutive replies without tools; a tool
//! round or user guidance starts it over ([`AnswerRecovery::reset`]).
use crate::model::{FinishReason, ModelError};

/// Most continuations of one answer in a row.
pub(super) const MAX_CONTINUATIONS: u32 = 3;

/// The loop's next move after a reply without tool calls.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Reply {
    /// The turn's answer: every continued part, joined. `cut_off` when the
    /// output limit still stopped it after the last continuation.
    Answer { text: String, cut_off: bool },
    /// Keep the partial reply and ask the model to continue it.
    Continue,
    /// Ask once more with `note`; keep the reply in the conversation only
    /// when `keep_reply` (it has text worth keeping).
    Retry {
        note: &'static str,
        keep_reply: bool,
    },
    /// The model gave no usable reply twice in a row.
    Fail(ModelError),
}

/// Recovery state across consecutive replies without tool calls.
#[derive(Debug, Default)]
pub(super) struct AnswerRecovery {
    /// Text of the replies continued so far.
    prefix: String,
    continuations: u32,
    /// An unusable reply was already asked for again.
    retried: bool,
}

impl AnswerRecovery {
    /// Classifies a reply that finished with `finish` and has `text`. On the
    /// turn's `final_call` no further call is possible, so the reply is the
    /// answer whatever its finish reason.
    pub(super) fn reply(&mut self, finish: FinishReason, text: &str, final_call: bool) -> Reply {
        let has_text = !text.trim().is_empty();
        if final_call {
            return self.answer(text, finish == FinishReason::Length);
        }
        match finish {
            FinishReason::Length if has_text => {
                if self.continuations >= MAX_CONTINUATIONS {
                    return self.answer(text, true);
                }
                self.continuations += 1;
                self.prefix.push_str(text);
                Reply::Continue
            }
            FinishReason::Length => self.retry(
                crate::prompts::output_limit_without_answer(),
                false,
                "The model used its whole output limit without answering, twice in a row. \
                 Raise the model's output limit or lower its reasoning effort, then retry.",
            ),
            FinishReason::MalformedToolCall => self.retry(
                crate::prompts::malformed_tool_call(),
                has_text,
                "The model sent a tool call the provider could not parse, twice in a row.",
            ),
            FinishReason::Stop | FinishReason::ToolCalls if has_text || !self.prefix.is_empty() => {
                self.answer(text, false)
            }
            FinishReason::Stop | FinishReason::ToolCalls => self.retry(
                crate::prompts::empty_answer(),
                false,
                "The model returned an empty reply twice in a row.",
            ),
        }
    }

    /// Starts over: the next reply begins a new answer.
    pub(super) fn reset(&mut self) {
        *self = Self::default();
    }

    fn answer(&mut self, text: &str, cut_off: bool) -> Reply {
        let mut joined = std::mem::take(&mut self.prefix);
        joined.push_str(text);
        self.reset();
        Reply::Answer {
            text: joined,
            cut_off,
        }
    }

    fn retry(&mut self, note: &'static str, keep_reply: bool, failure: &str) -> Reply {
        if self.retried {
            return Reply::Fail(ModelError::EmptyResponse(failure.to_owned()));
        }
        self.retried = true;
        Reply::Retry { note, keep_reply }
    }
}

#[cfg(test)]
mod tests;
