//! Shell job buffers, ownership, and persistent working-directory tests.
use super::*;

fn execute(
    broker: &ToolBroker,
    name: &str,
    args: &Value,
    access: builtin::Access,
) -> Result<Value> {
    let context = matches!(name, "shell_command" | "run_command")
        .then(|| prepare(broker, args))
        .transpose()?;
    super::execute(broker, Uuid::new_v4(), name, args, access, context)
}

#[test]
fn output_is_incremental_and_ring_is_bounded() {
    let mut output = OutputBuffer::default();
    output.append(b"first\n");
    assert_eq!(output.read(None).0, "first\n");
    assert!(output.read(None).0.is_empty());
    output.append(&vec![b'x'; OUTPUT_BYTES + 10]);
    let (text, lost) = output.read(None);
    assert_eq!(text.len(), OUTPUT_BYTES);
    assert!(lost);
}

#[test]
fn filtering_consumes_only_new_output() {
    let mut output = OutputBuffer::default();
    output.append(b"one\ntwo error\nthree\n");
    assert_eq!(output.read(Some("error")).0, "two error\n");
    assert!(output.read(None).0.is_empty());
}

#[test]
fn cwd_sentinel_is_removed_from_output_and_checked() {
    let root = tempfile::tempdir().unwrap();
    let sub = root.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    let marker = "__XYLO_PWD_test__";
    let mut capture = CwdCapture::new(marker.into());
    assert_eq!(capture.push(b"hello\n__XYLO_PWD_te"), b"hello\n");
    let tail = format!("st__{}\0", sub.display());
    assert!(capture.push(tail.as_bytes()).is_empty());
    assert_eq!(capture.finish().1, Some(sub));
}

#[test]
fn cwd_survives_a_new_registry_and_outside_is_reset() {
    let root = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let session = Uuid::new_v4();
    let sub = root.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    let registry = ShellJobs::default();
    registry
        .update_cwd(session, root.path(), data.path(), &sub)
        .unwrap();
    let reloaded = ShellJobs::default();
    assert_eq!(
        reloaded.cwd(session, root.path(), data.path()).unwrap(),
        sub.canonicalize().unwrap()
    );
    reloaded
        .update_cwd(session, root.path(), data.path(), data.path())
        .unwrap();
    assert_eq!(
        reloaded.cwd(session, root.path(), data.path()).unwrap(),
        root.path().canonicalize().unwrap()
    );
    assert!(!reloaded.take_notes(session).is_empty());
}

#[test]
fn shell_wrapper_preserves_exit_code_and_reports_final_cwd() {
    let root = tempfile::tempdir().unwrap();
    let sub = root.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    let command = wrapped_command("cd sub; printf hello; exit 7", "__XYLO_PWD_test__");
    let output = std::process::Command::new("/bin/sh")
        .args(["-c", &command])
        .current_dir(root.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(7));
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "hello");
    let mut capture = CwdCapture::new("__XYLO_PWD_test__".into());
    assert!(capture.push(&output.stderr).is_empty());
    assert_eq!(capture.finish().1, Some(sub.canonicalize().unwrap()));
}

#[test]
fn completion_notes_survive_worker_restart_and_are_consumed_once() {
    let data = tempfile::tempdir().unwrap();
    let id = Uuid::new_v4();
    let first = ShellJobs::default();
    first.configure(id, data.path());
    first.note(id, NoteKind::ShellJob, "job finished".into());
    let second = ShellJobs::default();
    second.configure(id, data.path());
    assert_eq!(
        second.take_notes(id),
        vec![(NoteKind::ShellJob, "job finished".into())]
    );
    let third = ShellJobs::default();
    third.configure(id, data.path());
    assert!(third.take_notes(id).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn background_output_waits_kill_and_conversation_isolation() {
    let (_directory, broker, _events) =
        super::super::tests::fixture(xylo_core::PermissionMode::FullAccess);
    let broker = Arc::new(broker);
    let other = broker.clone();
    let result = tokio::task::spawn_blocking(move || {
        execute(
            &other,
            "shell_command",
            &json!({"command":"printf 'ready\\n'; sleep 300", "run_in_background":true}),
            builtin::Access {
                outside: false,
                network: false,
            },
        )
    })
    .await
    .unwrap()
    .unwrap();
    let id = Uuid::parse_str(result["summary"]["job_id"].as_str().unwrap()).unwrap();
    assert!(broker.session.shell_jobs.kill(Uuid::new_v4(), id).is_err());
    let other = broker.clone();
    let output = tokio::task::spawn_blocking(move || {
        execute(
            &other,
            "shell_output",
            &json!({"job_id":id,"wait_seconds":15}),
            builtin::Access {
                outside: false,
                network: false,
            },
        )
    })
    .await
    .unwrap()
    .unwrap();
    assert!(
        output["summary"]["output"]
            .as_str()
            .unwrap()
            .contains("ready"),
        "{output}"
    );
    assert_eq!(output["summary"]["running"], true);
    broker
        .session
        .shell_jobs
        .kill(broker.request.session_id, id)
        .unwrap();
    broker.session.shell_jobs.shutdown().await;
    assert!(
        !broker
            .session
            .shell_jobs
            .find(broker.request.session_id, id)
            .unwrap()
            .state
            .lock()
            .snapshot
            .running
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shell_cd_applies_to_the_next_command() {
    let (directory, broker, _events) =
        super::super::tests::fixture(xylo_core::PermissionMode::FullAccess);
    std::fs::create_dir(directory.path().join("sub")).unwrap();
    let result = tokio::task::spawn_blocking(move || {
        let access = builtin::Access {
            outside: false,
            network: false,
        };
        execute(
            &broker,
            "shell_command",
            &json!({"command":"cd sub"}),
            access,
        )
        .unwrap();
        execute(&broker, "shell_command", &json!({"command":"pwd"}), access).unwrap()
    })
    .await
    .unwrap();
    assert_eq!(
        result["summary"]["stdout"].as_str().unwrap().trim(),
        directory
            .path()
            .join("sub")
            .canonicalize()
            .unwrap()
            .to_str()
            .unwrap()
    );
}

#[cfg(unix)]
#[test]
fn replaced_working_directory_resets_with_a_note() {
    let root = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let sub = root.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    let id = Uuid::new_v4();
    let jobs = ShellJobs::default();
    jobs.update_cwd(id, root.path(), data.path(), &sub).unwrap();
    std::fs::remove_dir(&sub).unwrap();
    std::os::unix::fs::symlink(outside.path(), &sub).unwrap();
    assert_eq!(
        jobs.cwd(id, root.path(), data.path()).unwrap(),
        root.path().canonicalize().unwrap()
    );
    assert!(!jobs.take_notes(id).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approved_local_server_can_listen_while_the_model_continues() {
    let (_directory, broker, _events) =
        super::super::tests::fixture(xylo_core::PermissionMode::FullAccess);
    let other = broker.clone();
    let result = tokio::task::spawn_blocking(move || execute(&other, "run_command", &json!({
        "program":"node","args":["-e","require('node:net').createServer().listen(0, '127.0.0.1', () => console.log('listening'))"],
        "run_in_background":true,"allow_local_binding":true,"network":true,"justification":"Run the project's local development server"
    }), builtin::Access { outside:false, network:true })).await.unwrap().unwrap();
    let id = Uuid::parse_str(result["summary"]["job_id"].as_str().unwrap()).unwrap();
    let other = broker.clone();
    let output = tokio::task::spawn_blocking(move || {
        execute(
            &other,
            "shell_output",
            &json!({"job_id":id,"wait_seconds":15}),
            builtin::Access {
                outside: false,
                network: false,
            },
        )
    })
    .await
    .unwrap()
    .unwrap();
    broker
        .session
        .shell_jobs
        .kill(broker.request.session_id, id)
        .unwrap();
    broker.session.shell_jobs.shutdown().await;
    assert!(
        output["summary"]["output"]
            .as_str()
            .unwrap()
            .contains("listening"),
        "{output}"
    );
    assert_eq!(output["summary"]["running"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_quiet_server_flushes_its_last_live_output() {
    let (_directory, broker, mut events) =
        super::super::tests::fixture(xylo_core::PermissionMode::FullAccess);
    let other = broker.clone();
    let result = tokio::task::spawn_blocking(move || execute(&other, "shell_command", &json!({"command":"sleep 0.2; printf 'first\\n'; sleep 0.02; printf 'last\\n'; sleep 300","run_in_background":true}), builtin::Access { outside:false, network:false })).await.unwrap().unwrap();
    let id = Uuid::parse_str(result["summary"]["job_id"].as_str().unwrap()).unwrap();
    let visible = tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(event) = events.recv().await {
            if let EngineEvent::ShellJobUpdated { job } = event
                && job.id == id
                && job.output.contains("last")
            {
                return true;
            }
        }
        false
    })
    .await;
    broker
        .session
        .shell_jobs
        .kill(broker.request.session_id, id)
        .unwrap();
    broker.session.shell_jobs.shutdown().await;
    assert!(
        visible.unwrap_or(false),
        "The final startup line never reached the live UI"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn monitor_waits_for_a_complete_line_instead_of_a_partial_chunk() {
    let (_directory, broker, _events) =
        super::super::tests::fixture(xylo_core::PermissionMode::FullAccess);
    let other = broker.clone();
    let result = tokio::task::spawn_blocking(move || execute(&other, "shell_command", &json!({"command":"printf part; sleep 0.4; printf 'ial\\n'; sleep 300","run_in_background":true}), builtin::Access { outside:false, network:false })).await.unwrap().unwrap();
    let id = Uuid::parse_str(result["summary"]["job_id"].as_str().unwrap()).unwrap();
    let job = broker
        .session
        .shell_jobs
        .find(broker.request.session_id, id)
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let mut changed = Box::pin(job.changed.notified());
            changed.as_mut().enable();
            if job.state.lock().output.unread() {
                break;
            }
            changed.await;
        }
    })
    .await
    .unwrap();
    let other = broker.clone();
    let output = tokio::task::spawn_blocking(move || {
        execute(
            &other,
            "shell_output",
            &json!({"job_id":id,"wait_seconds":2}),
            builtin::Access {
                outside: false,
                network: false,
            },
        )
    })
    .await
    .unwrap()
    .unwrap();
    broker
        .session
        .shell_jobs
        .kill(broker.request.session_id, id)
        .unwrap();
    broker.session.shell_jobs.shutdown().await;
    assert_eq!(output["summary"]["output"], "partial\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_ninth_running_job_is_refused_before_its_command_runs() {
    let (directory, broker, _events) =
        super::super::tests::fixture(xylo_core::PermissionMode::FullAccess);
    for index in 0..8 {
        let path = directory.path().join(format!("job-{index}.log"));
        broker
            .session
            .shell_jobs
            .state
            .lock()
            .jobs
            .push_back(Arc::new(Job {
                session: broker.request.session_id,
                background: true,
                cancel: CancellationToken::new(),
                changed: Notify::new(),
                finished: AtomicBool::new(false),
                publication: Mutex::new(()),
                state: Mutex::new(JobState {
                    snapshot: ShellJob {
                        action_id: None,
                        background: true,
                        id: Uuid::new_v4(),
                        turn_id: broker.request.turn_id,
                        command: "fixture".into(),
                        cwd: directory.path().into(),
                        log_path: path.clone(),
                        running: true,
                        exit_code: None,
                        output: String::new(),
                        error: None,
                    },
                    output: OutputBuffer::default(),
                    log: File::create(path).unwrap(),
                    capture: None,
                    update_pending: false,
                    network_hint: None,
                }),
            }));
    }
    let other = broker.clone();
    let error = tokio::task::spawn_blocking(move || {
        execute(
            &other,
            "shell_command",
            &json!({"command":"touch should-not-exist","run_in_background":true}),
            builtin::Access {
                outside: false,
                network: false,
            },
        )
    })
    .await
    .unwrap()
    .unwrap_err();
    assert!(error.downcast_ref::<code::Unapplied>().is_some());
    assert!(!directory.path().join("should-not-exist").exists());
    assert_eq!(broker.session.shell_jobs.state.lock().jobs.len(), 8);
}

async fn approval(
    events: &mut tokio::sync::mpsc::Receiver<EngineEvent>,
) -> (Uuid, Value, Option<String>) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let EngineEvent::ApprovalRequired {
                action_id,
                arguments,
                rule,
                ..
            } = events.recv().await.unwrap()
            {
                return (action_id, arguments, rule);
            }
        }
    })
    .await
    .expect("command did not request approval")
}

async fn finished(job: &Job) {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let mut changed = Box::pin(job.changed.notified());
            changed.as_mut().enable();
            if job.finished.load(Ordering::Acquire) {
                break;
            }
            changed.await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_ancestor_configuration_after_persisted_cd() {
    let (directory, broker, mut events) =
        super::super::tests::fixture(xylo_core::PermissionMode::ApproveForMe);
    std::fs::create_dir(directory.path().join("sub")).unwrap();
    std::fs::create_dir(directory.path().join(".cargo")).unwrap();
    std::fs::write(
        directory.path().join(".cargo/config.toml"),
        "[net]\noffline = true\n",
    )
    .unwrap();
    let task = broker.clone();
    tokio::task::spawn_blocking(move || {
        task.execute("cd", "shell_command", json!({"command":"cd sub"}))
    })
    .await
    .unwrap()
    .unwrap();
    let task = broker.clone();
    let pending = tokio::task::spawn_blocking(move || {
        task.execute(
            "fetch",
            "shell_command",
            json!({"command":"cargo fetch","network":true,"justification":"Download dependencies"}),
        )
    });
    let response = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let EngineEvent::ApprovalRequired {
                action_id, reason, ..
            } = events.recv().await.unwrap()
            {
                broker
                    .session
                    .approvals()
                    .decide_with(action_id, false, false)
                    .unwrap();
                return reason.unwrap();
            }
        }
    })
    .await;
    broker.cancel.cancel();
    let _ = pending.await;
    assert!(
        response
            .expect("ancestor cargo config bypassed review")
            .contains(".cargo/config.toml")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_execution_keeps_cwd_captured_before_approval() {
    let (directory, broker, mut events) =
        super::super::tests::fixture(xylo_core::PermissionMode::AskForApproval);
    std::fs::create_dir(directory.path().join("sub")).unwrap();
    let task = broker.clone();
    let started = tokio::task::spawn_blocking(move || task.execute("background-cd", "shell_command", json!({"command":"while [ ! -f release ]; do sleep 0.02; done; cd sub","run_in_background":true}))).await.unwrap().unwrap();
    let started: Value = serde_json::from_str(&started).unwrap();
    let id = Uuid::parse_str(started["summary"]["job_id"].as_str().unwrap()).unwrap();
    let task = broker.clone();
    let pending = tokio::task::spawn_blocking(move || {
        task.execute(
            "reviewed-pwd",
            "shell_command",
            json!({"command":"pwd","network":true,"justification":"Exercise reviewed command"}),
        )
    });
    let (action, _, _) = approval(&mut events).await;
    std::fs::write(directory.path().join("release"), "").unwrap();
    finished(
        &broker
            .session
            .shell_jobs
            .find(broker.request.session_id, id)
            .unwrap(),
    )
    .await;
    broker
        .session
        .approvals()
        .decide_with(action, true, false)
        .unwrap();
    let result = pending.await.unwrap().unwrap();
    let result: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(
        result["summary"]["stdout"].as_str().unwrap().trim(),
        directory.path().canonicalize().unwrap().to_str().unwrap()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_all_completions_survive_until_model_boundary() {
    let (_directory, broker, _events) =
        super::super::tests::fixture(xylo_core::PermissionMode::FullAccess);
    let mut ids = Vec::new();
    for index in 0..10 {
        let task = broker.clone();
        let result = tokio::task::spawn_blocking(move || {
            task.execute(
                &format!("short-{index}"),
                "shell_command",
                json!({"command":format!("printf done-{index}"),"run_in_background":true}),
            )
        })
        .await
        .unwrap()
        .unwrap();
        let result: Value = serde_json::from_str(&result).unwrap();
        let id = Uuid::parse_str(result["summary"]["job_id"].as_str().unwrap()).unwrap();
        finished(
            &broker
                .session
                .shell_jobs
                .find(broker.request.session_id, id)
                .unwrap(),
        )
        .await;
        ids.push(id);
    }
    broker.session.shell_jobs.note(
        broker.request.session_id,
        NoteKind::WorkingDirectory,
        "reset fixture".into(),
    );
    let notes = broker
        .session
        .shell_jobs
        .take_notes(broker.request.session_id);
    for (index, id) in ids.iter().enumerate() {
        assert!(
            notes.iter().any(|(_, text)| text.contains(&id.to_string())
                && text.contains("Some(0)")
                && text.contains(&format!("done-{index}"))),
            "completion lost for {id}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_outside_cwd_is_explicit_approved_and_retained() {
    let (_directory, broker, mut events) =
        super::super::tests::fixture(xylo_core::PermissionMode::AskForApproval);
    let outside = tempfile::tempdir().unwrap();
    let cwd = outside.path().canonicalize().unwrap();
    let task = broker.clone();
    let requested = cwd.clone();
    let pending = tokio::task::spawn_blocking(move || {
        task.execute(
            "outside-cwd",
            "shell_command",
            json!({"command":"pwd; printf approved > permitted","cwd":requested}),
        )
    });
    let (action, presented, rule) = approval(&mut events).await;
    broker
        .session
        .approvals()
        .decide_with(action, true, false)
        .unwrap();
    let result = pending.await.unwrap().unwrap();
    assert_eq!(presented["cwd"], json!(cwd));
    assert!(rule.is_none());
    assert!(broker.exposure().unwrap().untrusted());
    assert!(result.contains(cwd.to_str().unwrap()), "{result}");
    assert_eq!(
        std::fs::read_to_string(cwd.join("permitted")).unwrap(),
        "approved"
    );
    assert_eq!(
        broker
            .session
            .shell_jobs
            .cwd(
                broker.request.session_id,
                &broker.request.workspace,
                broker.artifacts.parent().unwrap()
            )
            .unwrap(),
        cwd
    );
}

#[test]
fn review_delayed_live_publication_cannot_follow_completion() {
    let directory = tempfile::tempdir().unwrap();
    let jobs = ShellJobs::default();
    let path = directory.path().join("log");
    let job = Arc::new(Job {
        session: Uuid::new_v4(),
        background: true,
        cancel: CancellationToken::new(),
        changed: Notify::new(),
        finished: AtomicBool::new(false),
        publication: Mutex::new(()),
        state: Mutex::new(JobState {
            snapshot: ShellJob {
                action_id: None,
                background: true,
                id: Uuid::new_v4(),
                turn_id: Uuid::new_v4(),
                command: "fixture".into(),
                cwd: directory.path().into(),
                log_path: path.clone(),
                running: true,
                exit_code: None,
                output: String::new(),
                error: None,
            },
            output: OutputBuffer::default(),
            log: File::create(path).unwrap(),
            capture: None,
            update_pending: false,
            network_hint: None,
        }),
    });
    let (captured, capture) = std::sync::mpsc::channel();
    let (release, resume) = std::sync::mpsc::channel();
    let (completed, completion) = std::sync::mpsc::channel();
    let resume = Mutex::new(resume);
    let snapshots = Arc::new(Mutex::new(Vec::new()));
    let delivered = snapshots.clone();
    jobs.set_events(Arc::new(move |_, event| {
        if let EngineEvent::ShellJobUpdated { job } = event {
            if job.running {
                captured.send(()).unwrap();
                resume.lock().recv_timeout(Duration::from_secs(5)).unwrap();
            }
            delivered.lock().push(job.running);
            if !job.running {
                completed.send(()).unwrap();
            }
        }
    }));
    let live_jobs = jobs.clone();
    let live_job = job.clone();
    let live = std::thread::spawn(move || live_jobs.publish(&live_job));
    capture.recv_timeout(Duration::from_secs(5)).unwrap();
    let finished_jobs = jobs.clone();
    let finished_job = job.clone();
    let root = directory.path().to_owned();
    let done = std::thread::spawn(move || {
        finished_jobs.finish(
            &finished_job,
            &Err(anyhow::anyhow!("fixture completion")),
            &root,
            &root,
            None,
        )
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while job.state.lock().snapshot.running && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    // Completion either reaches the sink or waits on the publication lock.
    let reached_sink = completion.recv_timeout(Duration::from_millis(100)).is_ok();
    let serialized = job.publication.is_locked();
    release.send(()).unwrap();
    live.join().unwrap();
    done.join().unwrap();
    assert!(
        reached_sink || serialized,
        "completion never reached the controlled interleaving"
    );
    assert!(!job.state.lock().snapshot.running);
    assert_eq!(
        *snapshots.lock(),
        vec![true, false],
        "an old live snapshot resurrected a finished job"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outside_shell_denial_and_dynamic_cd_cannot_grant_scope() {
    let (directory, broker, mut events) =
        super::super::tests::fixture(xylo_core::PermissionMode::AskForApproval);
    let outside = tempfile::tempdir().unwrap();
    let cwd = outside.path().canonicalize().unwrap();
    let args = json!({"command":"printf denied > marker", "cwd":cwd, "network":true,"justification":"Scope fixture"});
    if let Some((rule, _)) = super::super::permissions::approval_rule(
        "shell_command",
        &args,
        &super::super::permissions::Reach::Project,
        directory.path(),
    ) {
        broker.rules.lock().insert(rule);
    }
    let task = broker.clone();
    let pending =
        tokio::task::spawn_blocking(move || task.execute("denied-outside", "shell_command", args));
    let (action, _, rule) = approval(&mut events).await;
    broker
        .session
        .approvals()
        .decide_with(action, false, false)
        .unwrap();
    assert!(pending.await.unwrap().is_err());
    assert!(rule.is_none());
    assert!(!cwd.join("marker").exists());
    assert!(broker.session.shell_jobs.state.lock().outside.is_empty());
    let task = broker.clone();
    let outside_path = cwd.clone();
    let result = tokio::task::spawn_blocking(move || {
        task.execute(
            "dynamic-outside",
            "shell_command",
            json!({"command":format!("cd '{}'; printf denied > marker", outside_path.display())}),
        )
    })
    .await
    .unwrap()
    .unwrap_err()
    .to_string();
    assert!(!cwd.join("marker").exists(), "{result}");
    assert_eq!(
        broker
            .session
            .shell_jobs
            .cwd(
                broker.request.session_id,
                directory.path(),
                broker.artifacts.parent().unwrap()
            )
            .unwrap(),
        directory.path().canonicalize().unwrap()
    );
    assert!(
        broker
            .session
            .shell_jobs
            .take_notes(broker.request.session_id)
            .iter()
            .any(|(kind, _)| *kind == NoteKind::WorkingDirectory)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approved_outside_cwd_cannot_write_siblings_or_survive_a_root_change() {
    let (directory, broker, mut events) =
        super::super::tests::fixture(xylo_core::PermissionMode::AskForApproval);
    let outside = tempfile::tempdir().unwrap();
    let sibling = tempfile::tempdir().unwrap();
    let cwd = outside.path().canonicalize().unwrap();
    std::fs::create_dir(cwd.join("sub")).unwrap();
    let task = broker.clone();
    let requested = cwd.clone();
    let pending = tokio::task::spawn_blocking(move || {
        task.execute(
            "grant-outside",
            "shell_command",
            json!({"command":"cd sub; printf granted > permitted", "cwd":requested}),
        )
    });
    let (action, _, _) = approval(&mut events).await;
    broker
        .session
        .approvals()
        .decide_with(action, true, false)
        .unwrap();
    pending.await.unwrap().unwrap();
    let task = broker.clone();
    let escaped = sibling.path().canonicalize().unwrap();
    let command = format!(
        "pwd; printf denied > '{}'",
        escaped.join("denied").display()
    );
    let pending = tokio::task::spawn_blocking(move || {
        task.execute(
            "retained-outside",
            "shell_command",
            json!({"command":command}),
        )
    });
    let (action, presented, _) = approval(&mut events).await;
    broker
        .session
        .approvals()
        .decide_with(action, true, false)
        .unwrap();
    let result = pending.await.unwrap().unwrap_err().to_string();
    assert_eq!(presented["outside_scope"], json!(cwd));
    assert!(result.contains(cwd.join("sub").to_str().unwrap()));
    assert!(!escaped.join("denied").exists());
    let new_root = tempfile::tempdir().unwrap();
    assert_eq!(
        broker
            .session
            .shell_jobs
            .cwd(
                broker.request.session_id,
                new_root.path(),
                broker.artifacts.parent().unwrap()
            )
            .unwrap(),
        new_root.path().canonicalize().unwrap()
    );
    // Scope grants are deliberately not restored from persisted cwd data.
    let restarted = ShellJobs::default();
    assert_eq!(
        restarted
            .cwd(
                broker.request.session_id,
                directory.path(),
                broker.artifacts.parent().unwrap()
            )
            .unwrap(),
        directory.path().canonicalize().unwrap()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreground_output_arrives_before_failure_with_action_identity() {
    let (_directory, broker, _events) =
        super::super::tests::fixture(xylo_core::PermissionMode::FullAccess);
    let broker = Arc::new(broker);
    let (sender, mut receive) = tokio::sync::mpsc::unbounded_channel();
    broker
        .session
        .shell_jobs
        .set_events(Arc::new(move |session, event| {
            let _ = sender.send((session, event));
        }));
    let action = Uuid::new_v4();
    let owner = broker.request.session_id;
    let task = broker.clone();
    let running = tokio::task::spawn_blocking(move || {
        let args = json!({"command":"printf 'first\\n'; sleep 0.4; printf 'last\\n'; exit 7"});
        let context = prepare(&task, &args).unwrap();
        super::execute(
            &task,
            action,
            "shell_command",
            &args,
            builtin::Access {
                outside: false,
                network: false,
            },
            Some(context),
        )
    });
    let mut observed_live_output = false;
    loop {
        let (session, event) = tokio::time::timeout(Duration::from_secs(10), receive.recv())
            .await
            .unwrap()
            .unwrap();
        if let EngineEvent::ShellJobUpdated { job } = event {
            assert_eq!(session, owner);
            assert_eq!(job.action_id, Some(action));
            assert!(!job.background);
            assert!(job.output.len() <= UI_TAIL_BYTES + 3);
            if job.running && job.output.contains("first") {
                observed_live_output = true;
                assert!(!running.is_finished());
            }
            if !job.running {
                assert_eq!(job.exit_code, Some(7));
                assert!(job.output.contains("last"));
                break;
            }
        }
    }
    assert!(observed_live_output);
    assert_eq!(running.await.unwrap().unwrap()["status"], "failed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupted_foreground_commands_keep_journaled_output_after_reopen() {
    for cancelled in [false, true] {
        let (directory, broker, _events) =
            super::super::tests::fixture(xylo_core::PermissionMode::FullAccess);
        let (sender, mut receive) = tokio::sync::mpsc::unbounded_channel();
        broker
            .session
            .shell_jobs
            .set_events(Arc::new(move |_, event| {
                let _ = sender.send(event);
            }));
        let execution = broker.clone();
        let running = tokio::task::spawn_blocking(move || {
            execution.execute(
            "interrupted-output", "shell_command",
            json!({"command":"printf 'retained before interruption\\n'; sleep 30", "timeout_seconds":if cancelled { 30 } else { 1 }}),
        )
        });
        loop {
            let event = tokio::time::timeout(Duration::from_secs(10), receive.recv())
                .await
                .unwrap()
                .unwrap();
            if let EngineEvent::ShellJobUpdated { job } = event
                && job.output.contains("retained before interruption")
            {
                if cancelled {
                    broker.cancel.cancel();
                }
                break;
            }
        }
        assert!(running.await.unwrap().is_err());
        let action = broker.journaled("interrupted-output").unwrap().unwrap();
        let reopened =
            xylo_services::Store::open(directory.path().join("profile/xylo.db")).unwrap();
        let record = reopened
            .turn_action(
                broker.request.session_id,
                broker.request.turn_id,
                action.intent.id,
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            record.state, "outcome_unknown",
            "Interrupted writes must stay uncertain"
        );
        let result = record.result.unwrap();
        let job = Uuid::parse_str(result["summary"]["job_id"].as_str().unwrap()).unwrap();
        assert_eq!(
            result["summary"]["job_turn_id"],
            json!(broker.request.turn_id)
        );
        let retained = broker
            .artifacts
            .join("shell-jobs")
            .join(broker.request.session_id.to_string())
            .join(format!("{job}.log"));
        assert!(
            std::fs::read_to_string(retained)
                .unwrap()
                .contains("retained before interruption")
        );
        broker.session.shell_jobs.shutdown().await;
    }
}
