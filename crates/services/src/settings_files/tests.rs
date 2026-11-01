use super::*;
use serde_json::json;

#[test]
fn layered_rules_reload_and_managed_disable_survives() {
    let root = tempfile::tempdir().unwrap();
    let managed = root.path().join("managed.json");
    let project = root.path().join("project.json");
    std::fs::write(
        &managed,
        json!({"disableAllHooks":true,"permissions":{"deny":["Bash(rm:*)"]}}).to_string(),
    )
    .unwrap();
    std::fs::write(
        &project,
        json!({"disableAllHooks":false,"permissions":{"allow":["Bash(*)"]}}).to_string(),
    )
    .unwrap();
    let files = SettingsFiles::from_paths(
        root.path(),
        vec![(managed.clone(), true), (project.clone(), false)],
    );
    let settings = files.load().unwrap();
    assert_eq!(settings.value["disableAllHooks"], true);
    assert_eq!(
        settings.evaluate("shell_command", &json!({"command":"rm -rf src"})),
        Some(PermissionDecision::Deny)
    );
    assert_eq!(settings.rules[0].source, managed);
    std::fs::write(
        project,
        json!({"permissions":{"ask":["Edit(src/**)"]}}).to_string(),
    )
    .unwrap();
    assert_eq!(
        files
            .load()
            .unwrap()
            .evaluate("file_edit", &json!({"path":"src/main.rs"})),
        Some(PermissionDecision::Ask)
    );
}

#[test]
fn rules_accept_aliases_domains_mcp_and_shell_boundaries() {
    let settings = EffectiveSettings::from_value(json!({"permissions":{"allow":["Bash(npm test:*)","Read(src/**)","WebFetch(domain:docs.rs)","mcp__github__search"]}})).unwrap();
    assert_eq!(
        settings.evaluate("shell_command", &json!({"command":"npm test -- --watch"})),
        Some(PermissionDecision::Allow)
    );
    assert_eq!(
        settings.evaluate("shell_command", &json!({"command":"npm testing"})),
        None
    );
    assert_eq!(
        settings.evaluate("web_fetch", &json!({"url":"https://docs.rs/foo"})),
        Some(PermissionDecision::Allow)
    );
    assert_eq!(
        settings.evaluate("web_fetch", &json!({"url":"https://docs.rs.evil/foo"})),
        None
    );
    assert_eq!(
        settings.evaluate("mcp__github__search", &json!({})),
        Some(PermissionDecision::Allow)
    );
}

#[test]
fn malformed_settings_fail_closed_and_local_save_is_atomic() {
    let root = tempfile::tempdir().unwrap();
    let files = SettingsFiles::from_paths(root.path(), vec![]);
    files
        .save_rule("Bash(npm test:*)", PermissionDecision::Allow, false)
        .unwrap();
    assert!(root.path().join(".xylo/settings.local.json").is_file());
    assert!(
        std::fs::read_to_string(root.path().join(".gitignore"))
            .unwrap()
            .contains(".xylo/settings.local.json")
    );
    let bad = root.path().join("bad.json");
    std::fs::write(&bad, "{").unwrap();
    assert!(
        SettingsFiles::from_paths(root.path(), vec![(bad, false)])
            .load()
            .is_err()
    );
}

#[test]
fn hook_output_handles_updates_context_and_stop() {
    let output = crate::hooks::HookOutput::parse(&json!({"hookSpecificOutput":{"permissionDecision":"ask","updatedInput":{"path":"new"},"additionalContext":"context"},"continue":false,"stopReason":"stop"})).unwrap();
    assert_eq!(output.decision, Some(PermissionDecision::Ask));
    assert_eq!(output.updated_input, Some(json!({"path":"new"})));
    assert_eq!(output.context, vec!["context"]);
    assert_eq!(output.stop_reason.as_deref(), Some("stop"));
    assert!(crate::hooks::matches(Some("^(Read|Edit)$"), "file_read").unwrap());
    assert!(crate::hooks::matches(Some("["), "file_read").is_err());
}

#[test]
fn normalized_paths_and_managed_nested_disables_cannot_be_bypassed() {
    let root = tempfile::tempdir().unwrap();
    let managed = root.path().join("managed.json");
    let local = root.path().join("local.json");
    std::fs::write(
        &managed,
        json!({"nested":{"disableHooks":true},"permissions":{"deny":["Read(secret.txt)"]}})
            .to_string(),
    )
    .unwrap();
    std::fs::write(&local, json!({"nested":null}).to_string()).unwrap();
    let settings = SettingsFiles::from_paths(root.path(), vec![(managed, true), (local, false)])
        .load()
        .unwrap();
    assert_eq!(settings.value["nested"]["disableHooks"], true);
    assert_eq!(
        settings.evaluate("file_read", &json!({"path":root.path().join("secret.txt")})),
        Some(PermissionDecision::Deny)
    );
    assert_eq!(
        settings.evaluate("file_read", &json!({"path":"src/../secret.txt"})),
        Some(PermissionDecision::Deny)
    );
}

#[test]
fn fields_rules_do_not_need_to_persist_tool_content() {
    let settings = EffectiveSettings::from_value(
        json!({"permissions":{"allow":["file_write(fields:{\"path\":\"notes.txt\"})"]}}),
    )
    .unwrap();
    assert_eq!(
        settings.evaluate(
            "file_write",
            &json!({"path":"notes.txt","content":"private"})
        ),
        Some(PermissionDecision::Allow)
    );
    assert_eq!(
        settings.evaluate(
            "file_write",
            &json!({"path":"other.txt","content":"private"})
        ),
        None
    );
}

#[tokio::test]
async fn hook_deadlines_stop_execution_and_settings_are_protected() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join(".xylo")).unwrap();
    let path = root.path().join(".xylo/settings.json");
    std::fs::write(&path, "{}").unwrap();
    let command = crate::hooks::CommandHook {
        command: "sleep 5".into(),
        timeout: std::time::Duration::from_millis(100),
    };
    let error = crate::hooks::run(
        &command,
        &json!({}),
        root.path(),
        &[],
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("timed out"));
    let command = crate::hooks::CommandHook {
        command: "printf overwritten > .xylo/settings.json".into(),
        timeout: std::time::Duration::from_secs(5),
    };
    let _ = crate::hooks::run(
        &command,
        &json!({}),
        root.path(),
        &[],
        tokio_util::sync::CancellationToken::new(),
    )
    .await;
    assert_eq!(std::fs::read_to_string(path).unwrap(), "{}");
}

#[cfg(unix)]
#[test]
fn local_save_never_follows_a_gitignore_symlink() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("private");
    std::fs::write(&secret, "private data").unwrap();
    std::os::unix::fs::symlink(&secret, root.path().join(".gitignore")).unwrap();
    let result = SettingsFiles::from_paths(root.path(), vec![]).save_rule(
        "Read(*)",
        PermissionDecision::Allow,
        false,
    );
    assert!(
        result.is_err(),
        "The host must reject a symlinked ignore file"
    );
    assert_eq!(std::fs::read_to_string(secret).unwrap(), "private data");
    assert!(!root.path().join(".xylo/settings.local.json").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn private_hook_input_preserves_owner_close_cleanup() {
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
    let root = tempfile::tempdir().unwrap();
    let mut prepared = crate::sandbox::prepare_coding(
        "/bin/sh",
        &["-c".into(), "cat; echo ready; sleep 300".into()],
        root.path(),
        &[],
        &[],
    )
    .unwrap();
    crate::sandbox::set_child_input(&mut prepared, b"{\"fixture\":true}\n").unwrap();
    prepared
        .command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = prepared.command.spawn().unwrap();
    let owner = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        output.read_line(&mut line),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(line.trim(), "{\"fixture\":true}");
    line.clear();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        output.read_line(&mut line),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(line.trim(), "ready");
    drop(owner);
    let status = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(!status.success());
    let mut remaining = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        output.read_to_end(&mut remaining),
    )
    .await
    .unwrap()
    .unwrap();
}

#[test]
fn every_lifecycle_event_and_resolved_mcp_matcher_is_configured() {
    let mut value = json!({"hooks":{}});
    for event in crate::hooks::EVENTS {
        value["hooks"][*event] = json!([{"hooks":[{"command":"printf '{}'"}]}]);
    }
    let settings = EffectiveSettings::from_value(value).unwrap();
    for event in crate::hooks::EVENTS {
        assert_eq!(
            crate::hooks::configured(&settings, event, "")
                .unwrap()
                .len(),
            1
        );
    }
    let settings=EffectiveSettings::from_value(json!({"hooks":{"PreToolUse":[{"matcher":"^mcp__github__search$","hooks":[{"command":"printf '{}'"}]}]}})).unwrap();
    assert_eq!(
        crate::hooks::configured_alias(
            &settings,
            "PreToolUse",
            "connector_call",
            Some("mcp__github__search")
        )
        .unwrap()
        .len(),
        1
    );
}

#[tokio::test]
async fn private_hook_input_rejects_an_external_file_without_exposing_it() {
    use std::process::Stdio;
    use tokio::io::AsyncReadExt;
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(outside.path(), "not hook input").unwrap();
    let mut prepared =
        crate::sandbox::prepare_coding("/bin/cat", &[], root.path(), &[], &[]).unwrap();
    crate::sandbox::set_child_input(&mut prepared, b"{}").unwrap();
    prepared
        .command
        .env("XYLO_SANDBOX_CHILD_STDIN", outside.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    prepared.command.process_group(0);
    let mut child = prepared.command.spawn().unwrap();
    let _owner = child.stdin.take();
    let mut stdout = child.stdout.take().unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        stdout.read_to_end(&mut bytes),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!child.wait().await.unwrap().success());
    assert!(bytes.is_empty());
    assert!(crate::sandbox::read_start_status(&prepared.status_file).is_some());
}

#[test]
fn malformed_hook_controls_fail_closed() {
    let settings = EffectiveSettings::from_value(
        json!({"hooks":{"PreToolUse":[{"matcher":42,"hooks":[{"command":"printf '{}'"}]}]}}),
    )
    .unwrap();
    assert!(crate::hooks::configured(&settings, "PreToolUse", "file_write").is_err());
    assert!(EffectiveSettings::from_value(json!({"disableAllHooks":"true"})).is_err());
}

#[test]
fn review_f_compound_shell_prefix_keeps_deny_and_ask_but_not_narrow_allow() {
    for decision in [PermissionDecision::Deny, PermissionDecision::Ask] {
        let mut value = json!({"permissions":{"allow":["Bash(*)"]}});
        value["permissions"][decision.key()] = json!(["Bash(printf:*)"]);
        let settings = EffectiveSettings::from_value(value).unwrap();
        for command in [
            "printf safe; true",
            "printf; true",
            "true && printf safe",
            "printf safe | cat",
            "echo $(printf safe)",
            "printf safe\ntrue",
        ] {
            assert_eq!(
                settings.evaluate("shell_command", &json!({"command":command})),
                Some(decision),
                "{command}"
            );
        }
        assert_eq!(
            settings.evaluate(
                "shell_command",
                &json!({"command":"printf_other safe; true"})
            ),
            Some(PermissionDecision::Allow)
        );
    }
    let settings =
        EffectiveSettings::from_value(json!({"permissions":{"allow":["Bash(printf:*)"]}})).unwrap();
    assert_eq!(
        settings.evaluate("shell_command", &json!({"command":"printf safe; true"})),
        None
    );
    assert_eq!(
        settings.evaluate("shell_command", &json!({"command":"printf safe"})),
        Some(PermissionDecision::Allow)
    );
}
