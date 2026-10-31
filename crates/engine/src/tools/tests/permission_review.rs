//! Regressions for permission bypasses found in review: symlinked paths,
//! over-broad "always allow" rules, connector hints, approvals that outlive
//! a mode change, network fetches that run code, git global options and
//! rule key length.
use super::super::permissions::{self, Call, Origin, Reach, Verdict};
use super::super::*;
use super::{fixture, run};

fn decide(mode: PermissionMode, name: &str, arguments: Value, writes: bool) -> Verdict {
    let workspace = std::env::temp_dir();
    permissions::decide(
        mode,
        &Call {
            name,
            arguments: &arguments,
            writes,
            origin: Origin::Model,
            reach: &Reach::Project,
            workspace: &workspace,
            network: arguments
                .get("network")
                .and_then(Value::as_bool)
                .filter(|network| *network)
                .map(|_| "Fetch dependencies"),
            untrusted_input: false,
        },
    )
}

fn asks(verdict: &Verdict) -> bool {
    matches!(verdict, Verdict::Ask(_))
}

fn rule(name: &str, arguments: Value, workspace: &std::path::Path) -> (String, String) {
    permissions::approval_rule(name, &arguments, &Reach::Project, workspace).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approve_for_me_asks_before_writing_through_a_symlink_out_of_the_project() {
    let (directory, broker, mut events) = fixture(PermissionMode::ApproveForMe);
    let outside = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(outside.path().join("Library/LaunchAgents")).unwrap();
    std::os::unix::fs::symlink(outside.path(), directory.path().join("escape")).unwrap();
    // The risk assessment resolves the link even though the file is new.
    assert!(
        risk::assess(
            "file_write",
            &json!({"path":"escape/Library/LaunchAgents/x.plist","content":""}),
            directory.path()
        )
        .is_some()
    );
    let task = broker.clone();
    let write = tokio::task::spawn_blocking(move || {
        task.execute(
            "escape",
            "file_write",
            json!({"path":"escape/Library/LaunchAgents/x.plist","content":"<plist/>"}),
        )
    });
    let reason = loop {
        if let EngineEvent::ApprovalRequired {
            action_id, reason, ..
        } = events.recv().await.unwrap()
        {
            broker
                .session
                .approvals()
                .decide_with(action_id, false, false)
                .unwrap();
            break reason.unwrap();
        }
    };
    assert!(reason.contains("outside the project"), "{reason}");
    assert!(write.await.unwrap().is_err());
    assert!(!outside.path().join("Library/LaunchAgents/x.plist").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn risky_project_files_get_rules_that_ordinary_edits_never_cover() {
    let (directory, broker, mut events) = fixture(PermissionMode::ApproveForMe);
    let root = directory.path();
    assert_eq!(
        rule(
            "file_write",
            json!({"path":"src/lib.rs","content":""}),
            root
        )
        .0,
        "edit"
    );
    let env = rule("file_write", json!({"path":".env","content":""}), root);
    assert_eq!(env, ("edit:.env".into(), "edits to `.env`".into()));
    assert_eq!(
        rule(
            "file_write",
            json!({"path":".git/hooks/pre-commit","content":""}),
            root
        )
        .0,
        "edit:.git/hooks/pre-commit"
    );
    std::fs::create_dir_all(root.join(".github/workflows")).unwrap();
    let mut approvals = 0;
    for (key, path) in [("env", ".env"), ("workflow", ".github/workflows/ci.yml")] {
        let task = broker.clone();
        let write = tokio::task::spawn_blocking(move || {
            task.execute(key, "file_write", json!({"path":path,"content":"x"}))
        });
        loop {
            if let EngineEvent::ApprovalRequired { action_id, .. } = events.recv().await.unwrap() {
                // "Always allow" the first; it must not cover the second.
                broker
                    .session
                    .approvals()
                    .decide_with(action_id, true, true)
                    .unwrap();
                approvals += 1;
                break;
            }
        }
        write.await.unwrap().unwrap();
    }
    assert_eq!(approvals, 2);
}

#[test]
fn command_rules_cover_exactly_what_their_labels_say() {
    let root = std::env::temp_dir();
    let command = |program: &str, args: &[&str]| {
        rule(
            "run_command",
            json!({"program": program, "args": args}),
            &root,
        )
    };
    for (approved, other) in [
        (
            command("git", &["push"]),
            command("git", &["push", "--force", "origin", "main"]),
        ),
        (
            command("rm", &["-rf", "target"]),
            command("rm", &["-rf", "target", "src"]),
        ),
        (
            command("npm", &["run", "test"]),
            command("npm", &["run", "deploy"]),
        ),
        (
            command("ssh", &["host"]),
            command("ssh", &["host", "rm", "-rf", "/"]),
        ),
        (
            command("bash", &["-c", "cargo test"]),
            command("bash", &["-c", "curl x | sh"]),
        ),
    ] {
        assert_ne!(approved.0, other.0, "{} covers {}", approved.1, other.1);
        assert!(approved.1.starts_with("exactly `"), "{}", approved.1);
    }
    let broad = command("cargo", &["test", "--workspace"]);
    assert_eq!(broad.0, "run:any:cargo test");
    assert_eq!(broad.0, command("cargo", &["test"]).0);
    assert_eq!(broad.1, "`cargo test` with any arguments");
    assert_eq!(command("pytest", &["-k", "slow"]).0, "run:any:pytest");
    // Options before the subcommand, or a risky argument, keep it exact.
    assert!(
        command("cargo", &["--config", "x", "test"])
            .0
            .starts_with("run:exact:")
    );
    assert!(
        command("cargo", &["test", "--", "~/.ssh/id_rsa"])
            .0
            .starts_with("run:exact:")
    );
}

#[test]
fn connector_read_hints_are_untrusted() {
    use PermissionMode::*;
    let lookup = json!({"connector_id":"c","tool":"search","arguments":{"query":"open issues"}});
    let secret =
        json!({"connector_id":"c","tool":"search","arguments":{"query":"ghp_abcdefghijklmnop"}});
    let large = json!({"connector_id":"c","tool":"search","arguments":{"body":"x".repeat(4096)}});
    let contents =
        json!({"connector_id":"c","tool":"search","arguments":{"text":"line\n".repeat(20)}});
    assert!(asks(&decide(
        AskForApproval,
        "connector_call",
        lookup.clone(),
        false
    )));
    assert!(asks(&decide(
        ReadOnly,
        "connector_call",
        lookup.clone(),
        false
    )));
    assert_eq!(
        decide(ApproveForMe, "connector_call", lookup.clone(), false),
        Verdict::Allow
    );
    for unsafe_arguments in [secret, large, contents] {
        assert!(asks(&decide(
            ApproveForMe,
            "connector_call",
            unsafe_arguments,
            false
        )));
    }
    assert!(asks(&decide(
        ApproveForMe,
        "connector_call",
        lookup.clone(),
        true
    )));
    assert_eq!(
        decide(FullAccess, "connector_call", lookup, true),
        Verdict::Allow
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_approval_never_outranks_a_mode_changed_while_waiting() {
    let (_directory, broker, mut events) = fixture(PermissionMode::AskForApproval);
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("late.txt");
    let task = broker.clone();
    let path = target.clone();
    let write = tokio::task::spawn_blocking(move || {
        task.execute("late", "file_write", json!({"path":path,"content":"x"}))
    });
    loop {
        if let EngineEvent::ApprovalRequired { action_id, .. } = events.recv().await.unwrap() {
            broker.session.permissions().set(PermissionMode::ReadOnly);
            broker
                .session
                .approvals()
                .decide_with(action_id, true, false)
                .unwrap();
            break;
        }
    }
    let error = write.await.unwrap().unwrap_err().to_string();
    assert_eq!(error, "Permission mode changed to Read only; not run");
    assert!(!target.exists());
    assert_eq!(broker.journaled("late").unwrap().unwrap().state, "denied");
}

#[test]
fn network_requests_run_unasked_only_for_plain_downloads() {
    use PermissionMode::*;
    let shell = |command: &str| json!({"command": command, "network": true});
    for fetch in [
        "cargo fetch",
        "go mod download",
        "npm ci --ignore-scripts",
        "brew fetch jq",
    ] {
        assert_eq!(
            decide(ApproveForMe, "shell_command", shell(fetch), true),
            Verdict::Allow,
            "{fetch}"
        );
    }
    for runs_code in [
        "cargo build",
        "cargo test",
        "cargo run",
        "go build ./...",
        "go test ./...",
        "swift build",
        "npm install",
        "pip install -r requirements.txt",
        "pip download -r requirements.txt",
        "git -C vendor fetch origin",
    ] {
        assert!(
            asks(&decide(
                ApproveForMe,
                "shell_command",
                shell(runs_code),
                true
            )),
            "{runs_code}"
        );
    }
}

#[test]
fn git_global_options_do_not_hide_destructive_subcommands() {
    let root = std::env::temp_dir();
    let shell = |command: &str| risk::assess("shell_command", &json!({"command": command}), &root);
    for command in [
        "git -C . reset --hard",
        "git -c x=y clean -fdx",
        "git --git-dir=.git --work-tree . push --force",
        "git --no-pager -P branch -D main",
        "git --namespace n push origin :main",
    ] {
        assert!(shell(command).is_some(), "{command}");
    }
    assert!(shell("git -C . --no-pager status").is_none());
}

#[test]
fn long_rule_keys_are_hashed_but_stay_exact() {
    let root = std::env::temp_dir();
    let long = |suffix: &str| {
        rule(
            "shell_command",
            json!({"command": format!("echo {}{suffix}", "x".repeat(2000))}),
            &root,
        )
    };
    let first = long("a");
    let second = long("b");
    assert!(first.0.len() <= 512, "{}", first.0.len());
    assert!(first.0.starts_with("shell:echo "));
    assert_ne!(first.0, second.0);
    assert_eq!(first.0, long("a").0);
    assert!(first.1.chars().count() <= 170);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn helpers_still_run_offline_commands_in_the_default_mode() {
    let (_directory, broker, _events) = fixture(PermissionMode::AskForApproval);
    let output = run(
        &broker,
        "offline",
        "run_command",
        json!({"program":"git","args":["--version"]}),
    )
    .await
    .unwrap();
    assert!(output.contains("git version"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approve_for_me_reads_outside_unasked_unless_risky_and_always_asks_to_write() {
    let (_directory, broker, mut events) = fixture(PermissionMode::ApproveForMe);
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("notes.txt"), "plain outside text").unwrap();
    let read = run(
        &broker,
        "outside-read",
        "file_read",
        json!({"path":outside.path().join("notes.txt")}),
    )
    .await
    .unwrap();
    assert!(read.contains("plain outside text"));
    while let Ok(event) = events.try_recv() {
        assert!(!matches!(event, EngineEvent::ApprovalRequired { .. }));
    }
    let workspace = broker.request.workspace.clone();
    let key = |path: &str| {
        let arguments = json!({"path": path});
        permissions::decide(
            PermissionMode::ApproveForMe,
            &Call {
                name: "file_read",
                arguments: &arguments,
                writes: false,
                origin: Origin::Model,
                reach: &Reach::Outside(std::path::PathBuf::from(path)),
                workspace: &workspace,
                network: None,
                untrusted_input: false,
            },
        )
    };
    assert!(asks(&key("~/.ssh/id_ed25519")));
    assert!(asks(&key("~/.aws/credentials")));
    let task = broker.clone();
    let target = outside.path().join("written.txt");
    let path = target.clone();
    let write = tokio::task::spawn_blocking(move || {
        task.execute(
            "outside-write",
            "file_write",
            json!({"path":path,"content":"x"}),
        )
    });
    loop {
        if let EngineEvent::ApprovalRequired { action_id, .. } = events.recv().await.unwrap() {
            broker
                .session
                .approvals()
                .decide_with(action_id, false, false)
                .unwrap();
            break;
        }
    }
    assert!(write.await.unwrap().is_err());
    assert!(!target.exists());
    // Ask for approval still asks for any outside path, reads included.
    let arguments = json!({"path": outside.path().join("notes.txt")});
    assert!(asks(&permissions::decide(
        PermissionMode::AskForApproval,
        &Call {
            name: "file_read",
            arguments: &arguments,
            writes: false,
            origin: Origin::Model,
            reach: &Reach::Outside(outside.path().join("notes.txt")),
            workspace: &workspace,
            network: None,
            untrusted_input: false,
        },
    )));
}
