use super::*;

/// One step of a single call answered by `result`.
fn step(name: &str, arguments: &str, result: &str) -> Step {
    let call = ToolCall::new("c1", name, arguments);
    Step::new(&[call], &[Message::tool_result("c1", result)])
}

fn ok(text: &str) -> String {
    serde_json::json!({"status":"success","summary":text}).to_string()
}

fn failed(text: &str) -> String {
    serde_json::json!({"status":"error","summary":text}).to_string()
}

#[test]
fn identical_steps_escalate_from_note_to_warning_to_stop() {
    let mut detector = StallDetector::default();
    let read = || step("file_read", r#"{"path":"a.rs"}"#, &ok("same"));
    assert_eq!(detector.observe(read()), None);
    assert_eq!(detector.observe(read()), None);
    let pattern = Pattern::RepeatedCall {
        tools: vec!["file_read".into()],
        count: 3,
    };
    assert_eq!(
        detector.observe(read()),
        Some(Verdict::Note(pattern.clone()))
    );
    // The history restarted from the latest step: two more repeats strike.
    assert_eq!(detector.observe(read()), None);
    assert_eq!(
        detector.observe(read()),
        Some(Verdict::Warning(pattern.clone()))
    );
    assert_eq!(detector.observe(read()), None);
    assert_eq!(detector.observe(read()), Some(Verdict::Stop(pattern)));
}

#[test]
fn argument_key_order_and_spacing_do_not_hide_a_repeat() {
    let mut detector = StallDetector::default();
    detector.observe(step(
        "search_project",
        r#"{"query":"x","path":"src"}"#,
        &ok("r"),
    ));
    detector.observe(step(
        "search_project",
        r#"{ "path": "src", "query": "x" }"#,
        &ok("r"),
    ));
    assert!(matches!(
        detector.observe(step(
            "search_project",
            r#"{"path":"src","query":"x"}"#,
            &ok("r")
        )),
        Some(Verdict::Note(Pattern::RepeatedCall { count: 3, .. }))
    ));
}

#[test]
fn the_same_call_with_changing_results_needs_four_repeats() {
    let mut detector = StallDetector::default();
    for round in 0..3 {
        let result = ok(&format!("build {round}"));
        assert_eq!(
            detector.observe(step("shell_command", r#"{"command":"make"}"#, &result)),
            None
        );
    }
    assert!(matches!(
        detector.observe(step(
            "shell_command",
            r#"{"command":"make"}"#,
            &ok("build 3")
        )),
        Some(Verdict::Note(Pattern::RepeatedCall { count: 4, .. }))
    ));
}

#[test]
fn different_calls_are_progress_and_clear_strikes() {
    let mut detector = StallDetector::default();
    let read = || step("file_read", r#"{"path":"a.rs"}"#, &ok("same"));
    for _ in 0..3 {
        detector.observe(read());
    }
    assert_eq!(detector.strikes, 1);
    for index in 0..CALM_STEPS {
        let arguments = format!(r#"{{"path":"file{index}.rs"}}"#);
        assert_eq!(
            detector.observe(step("file_read", &arguments, &ok("x"))),
            None
        );
    }
    assert_eq!(detector.strikes, 0);
    for _ in 0..2 {
        detector.observe(read());
    }
    // A fresh pattern starts again at a note, not where it left off.
    assert!(matches!(detector.observe(read()), Some(Verdict::Note(_))));
}

#[test]
fn an_a_b_alternation_with_the_same_results_is_a_loop() {
    let mut detector = StallDetector::default();
    let a = || step("file_read", r#"{"path":"a.rs"}"#, &ok("A"));
    let b = || step("file_read", r#"{"path":"b.rs"}"#, &ok("B"));
    for _ in 0..2 {
        assert_eq!(detector.observe(a()), None);
        assert_eq!(detector.observe(b()), None);
    }
    assert_eq!(detector.observe(a()), None);
    assert_eq!(
        detector.observe(b()),
        Some(Verdict::Note(Pattern::Alternating {
            tools: vec!["file_read".into()],
            count: 3
        }))
    );
}

#[test]
fn an_edit_and_test_cycle_with_new_results_is_not_a_loop() {
    let mut detector = StallDetector::default();
    for round in 0..6 {
        let edit = format!(r#"{{"path":"a.rs","new":"v{round}"}}"#);
        assert_eq!(
            detector.observe(step("file_edit", &edit, &ok("edited"))),
            None
        );
        let output = ok(&format!("{} passed", round + 1));
        assert_eq!(
            detector.observe(step(
                "shell_command",
                r#"{"command":"cargo test"}"#,
                &output
            )),
            None
        );
    }
}

#[test]
fn the_same_error_from_different_arguments_is_not_a_loop() {
    let mut detector = StallDetector::default();
    for path in ["a", "b", "c", "d"] {
        let arguments = format!(r#"{{"path":"{path}"}}"#);
        let error = failed("no such file");
        assert_eq!(
            detector.observe(step("file_read", &arguments, &error)),
            None
        );
    }
}

#[test]
fn the_same_failing_call_between_other_steps_escalates_to_a_stop() {
    let mut detector = StallDetector::default();
    let test = |seconds: &str| {
        let output = failed(&format!("1 failed in {seconds}s"));
        step("shell_command", r#"{"command":"cargo test"}"#, &output)
    };
    let read = |path: &str| step("file_read", &format!(r#"{{"path":"{path}"}}"#), &ok("x"));
    let pattern = Pattern::RepeatedError {
        tool: "shell_command".into(),
        count: 3,
    };
    let mut verdicts = Vec::new();
    for round in 0..7 {
        if let Some(verdict) = detector.observe(test(&format!("0.{round}"))) {
            verdicts.push(verdict);
        }
        // Reading files changes nothing in the workspace.
        assert_eq!(detector.observe(read(&format!("f{round}.rs"))), None);
    }
    assert_eq!(
        verdicts,
        [
            Verdict::Note(pattern.clone()),
            Verdict::Warning(pattern.clone()),
            Verdict::Stop(pattern)
        ]
    );
}

#[test]
fn an_edit_and_test_cycle_with_the_same_failure_never_escalates() {
    let mut detector = StallDetector::default();
    let failure = failed("test parse::tests::ok ... FAILED");
    for round in 0..12 {
        let edit = format!(r#"{{"path":"src/parse.rs","new":"attempt {round}"}}"#);
        let diff =
            serde_json::json!({"diff":"@@ -1 +1 @@","added":1,"removed":1,"path":"src/parse.rs"});
        assert_eq!(
            detector.observe(step("file_edit", &edit, &diff.to_string())),
            None
        );
        assert_eq!(
            detector.observe(step(
                "shell_command",
                r#"{"command":"cargo test"}"#,
                &failure
            )),
            None
        );
    }
}

#[test]
fn an_edit_that_changes_nothing_is_not_progress() {
    let mut detector = StallDetector::default();
    let failure = failed("still failing");
    let unchanged = serde_json::json!({"diff":"","added":0,"removed":0}).to_string();
    let mut verdicts = Vec::new();
    for round in 0..3 {
        let edit = format!(r#"{{"path":"a.rs","new":"same {round}"}}"#);
        assert_eq!(detector.observe(step("file_edit", &edit, &unchanged)), None);
        verdicts.extend(detector.observe(step("shell_command", r#"{"command":"make"}"#, &failure)));
    }
    assert!(matches!(
        verdicts[..],
        [Verdict::Note(Pattern::RepeatedError { .. })]
    ));
}

#[test]
fn file_changes_are_read_from_successful_diff_results() {
    assert!(changed_file(r#"{"diff":"x","added":2,"removed":0}"#));
    assert!(changed_file(
        r#"{"diff":"","added":0,"removed":0,"created":true}"#
    ));
    assert!(!changed_file(r#"{"diff":"","added":0,"removed":0}"#));
    assert!(!changed_file(r#"{"status":"error","added":3}"#));
    assert!(!changed_file("not json"));
}

#[test]
fn shell_failures_and_flagged_observations_count_as_errors() {
    let shell = serde_json::json!({"status":"failed","exit_code":1,"summary":{"stderr":"boom"}});
    assert!(error_text(&shell.to_string()).unwrap().contains("boom"));
    assert!(error_text(r#"{"isError":true,"content":"bad"}"#).is_some());
    assert!(error_text(r#"{"success":false}"#).is_some());
    assert!(error_text(&ok("fine")).is_none());
    assert!(error_text("plain text").is_none());
}

#[test]
fn durations_are_normalized_but_other_numbers_are_kept() {
    assert_eq!(
        without_durations("1 failed in 0.52s; took 12 ms at line 40, x86"),
        "1 failed in #s; took #ms at line 40, x86"
    );
}

#[test]
fn patterns_describe_themselves_for_the_model_and_the_user() {
    let pattern = Pattern::RepeatedCall {
        tools: vec!["file_read".into(), "git_status".into()],
        count: 3,
    };
    assert_eq!(
        pattern.describe(),
        "You have made the same `file_read` + `git_status` call 3 times in a row."
    );
    assert_eq!(
        pattern.summary(),
        "the same `file_read` + `git_status` call 3 times in a row"
    );
}
