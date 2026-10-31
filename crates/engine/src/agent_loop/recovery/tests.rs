use super::*;
use FinishReason::{Length, MalformedToolCall, Stop};

fn answer(text: &str, cut_off: bool) -> Reply {
    Reply::Answer {
        text: text.into(),
        cut_off,
    }
}

#[test]
fn a_complete_reply_is_the_answer() {
    let mut recovery = AnswerRecovery::default();
    assert_eq!(recovery.reply(Stop, "Done.", false), answer("Done.", false));
}

#[test]
fn cut_off_replies_are_continued_and_joined_up_to_the_limit() {
    let mut recovery = AnswerRecovery::default();
    for part in ["a", "b", "c"] {
        assert_eq!(recovery.reply(Length, part, false), Reply::Continue);
    }
    // The fourth cut-off part is returned as it stands.
    assert_eq!(recovery.reply(Length, "d", false), answer("abcd", true));
    // The next answer starts over.
    assert_eq!(recovery.reply(Length, "x", false), Reply::Continue);
    assert_eq!(recovery.reply(Stop, "y", false), answer("xy", false));
}

#[test]
fn an_empty_continuation_ends_the_answer_with_the_parts_so_far() {
    let mut recovery = AnswerRecovery::default();
    assert_eq!(recovery.reply(Length, "Part one", false), Reply::Continue);
    assert_eq!(recovery.reply(Stop, "", false), answer("Part one", false));
}

#[test]
fn an_empty_reply_is_retried_once_then_fails() {
    let mut recovery = AnswerRecovery::default();
    assert_eq!(
        recovery.reply(Stop, "  ", false),
        Reply::Retry {
            note: crate::prompts::empty_answer(),
            keep_reply: false
        }
    );
    let Reply::Fail(error) = recovery.reply(Stop, "", false) else {
        panic!("a second empty reply must fail");
    };
    assert!(matches!(error, ModelError::EmptyResponse(_)));
    assert!(error.retryable());
    assert_eq!(error.kind(), xylo_core::ErrorKind::EmptyResponse);
}

#[test]
fn reasoning_that_used_the_whole_limit_gets_its_own_note() {
    let mut recovery = AnswerRecovery::default();
    assert_eq!(
        recovery.reply(Length, "", false),
        Reply::Retry {
            note: crate::prompts::output_limit_without_answer(),
            keep_reply: false
        }
    );
    let Reply::Fail(error) = recovery.reply(Length, "", false) else {
        panic!("a second reply without text must fail");
    };
    assert!(error.to_string().contains("output limit"));
}

#[test]
fn a_malformed_tool_call_is_asked_for_again_keeping_its_text() {
    let mut recovery = AnswerRecovery::default();
    assert_eq!(
        recovery.reply(MalformedToolCall, "Let me edit it.", false),
        Reply::Retry {
            note: crate::prompts::malformed_tool_call(),
            keep_reply: true
        }
    );
    recovery.reset();
    // After a tool round the retry is available again.
    assert!(matches!(
        recovery.reply(MalformedToolCall, "", false),
        Reply::Retry {
            keep_reply: false,
            ..
        }
    ));
}

#[test]
fn the_final_call_always_answers() {
    let mut recovery = AnswerRecovery::default();
    assert_eq!(recovery.reply(Length, "a", false), Reply::Continue);
    assert_eq!(recovery.reply(Length, "b", true), answer("ab", true));
    assert_eq!(recovery.reply(Stop, "", true), answer("", false));
}
