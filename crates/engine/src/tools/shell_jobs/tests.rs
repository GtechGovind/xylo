//! Shell job buffers, ownership, and persistent working-directory tests.
use super::*;

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
                state: Mutex::new(JobState {
                    snapshot: ShellJob {
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
