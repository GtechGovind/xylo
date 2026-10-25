use super::*;

pub(super) fn terminal_state(columns: usize, rows: usize) -> (Arc<Mutex<State>>, Listener) {
    let listener = Listener::default();
    let state = State {
        term: Term::new(Config::default(), &Size { columns, rows }, listener.clone()),
        title: "Terminal".into(),
        search_status: String::new(),
        exited: false,
        error: None,
    };
    (Arc::new(Mutex::new(state)), listener)
}

pub(super) fn assert_same_frame(actual: &TerminalSnapshot, expected: &TerminalSnapshot) {
    assert_eq!(
        (actual.columns, actual.rows),
        (expected.columns, expected.rows)
    );
    assert_eq!(actual.cursor, expected.cursor);
    assert_eq!(actual.display_offset, expected.display_offset);
    assert_eq!(actual.bracketed_paste, expected.bracketed_paste);
    assert_eq!(actual.application_cursor, expected.application_cursor);
    assert_eq!(actual.title, expected.title);
    assert_eq!(actual.selection, expected.selection);
    assert_eq!(actual.search_status, expected.search_status);
    assert_eq!(actual.exited, expected.exited);
    assert_eq!(actual.error, expected.error);
    assert_eq!(actual.cells.len(), expected.cells.len());
    for (actual, expected) in actual.cells.iter().zip(expected.cells.iter()) {
        assert_eq!(
            (
                &actual.text,
                actual.foreground,
                actual.background,
                actual.bold,
                actual.italic,
                actual.underline,
                actual.wide_spacer,
                actual.selected,
            ),
            (
                &expected.text,
                expected.foreground,
                expected.background,
                expected.bold,
                expected.italic,
                expected.underline,
                expected.wide_spacer,
                expected.selected,
            )
        );
    }
}

#[test]
fn burst_is_coalesced_before_capture_and_final_flush_keeps_complete_parser_state() {
    let (state, listener) = terminal_state(30, 5);
    let (frames, incoming) = mpsc::channel();
    let signal = Arc::new(FrameSignal::default());
    let publisher_signal = signal.clone();
    let published_state = state.clone();
    let publisher = thread::spawn(move || {
        publisher_signal.publish(Duration::from_secs(3600), || {
            frames.send(snapshot(&published_state.lock())).unwrap();
        });
    });
    signal.request(false);
    incoming.recv_timeout(Duration::from_secs(2)).unwrap();
    let text = "\x1b]2;Burst title\x07\x1b[?1h\x1b[?2004h\x1b[31m界e\u{301}\x1b[0m\r\n";
    let bytes = text.repeat(80).into_bytes();
    let (commands, _receiver) = mpsc::sync_channel(64);
    let mut parser = alacritty_terminal::vte::ansi::Processor::new();
    for chunk in bytes.chunks(7) {
        parse_output(&mut parser, &state, &listener, chunk, &commands, &signal).unwrap();
    }
    // The hour-long normal interval makes this check independent of machine
    // speed: none of the burst requests may construct an intermediate frame.
    assert!(incoming.try_recv().is_err());
    state.lock().exited = true;
    signal.request(true);
    let final_frame = incoming.recv_timeout(Duration::from_secs(2)).unwrap();
    let (expected, expected_listener) = terminal_state(30, 5);
    let mut expected_parser: alacritty_terminal::vte::ansi::Processor =
        alacritty_terminal::vte::ansi::Processor::new();
    expected_parser.advance(&mut expected.lock().term, &bytes);
    expected.lock().title = expected_listener.0.lock().title.take().unwrap();
    expected.lock().exited = true;
    assert_same_frame(&final_frame, &snapshot(&expected.lock()));
    assert!(
        final_frame
            .cells
            .iter()
            .any(|cell| cell.text.contains('界'))
    );
    assert!(final_frame.application_cursor && final_frame.bracketed_paste);
    signal.close();
    publisher.join().unwrap();
    assert!(incoming.try_recv().is_err());
}

fn wait_until_dirty(signal: &FrameSignal) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !signal.pending.lock().dirty {
        assert!(
            Instant::now() < deadline,
            "Parser did not reach reply dispatch"
        );
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn full_writer_queue_preserves_user_input_and_every_protocol_reply_without_holding_state() {
    let (state, listener) = terminal_state(80, 24);
    let signal = Arc::new(FrameSignal::default());
    let (commands, incoming) = mpsc::sync_channel(1);
    commands
        .send(Command::Write(b"user input".to_vec()))
        .unwrap();
    let parser_state = state.clone();
    let parser_signal = signal.clone();
    let (finished, completion) = mpsc::channel();
    let worker = thread::spawn(move || {
        let result = parse_output(
            &mut alacritty_terminal::vte::ansi::Processor::new(),
            &parser_state,
            &listener,
            b"\x1b[5n\x1b[6n",
            &commands,
            &parser_signal,
        );
        finished.send(result.is_ok()).unwrap();
    });
    wait_until_dirty(&signal);
    assert!(
        state.try_lock().is_some(),
        "Reply backpressure held terminal state"
    );
    for expected in [b"user input".as_slice(), b"\x1b[0n", b"\x1b[1;1R"] {
        let Command::Write(bytes) = incoming.recv_timeout(Duration::from_secs(2)).unwrap() else {
            panic!("Expected the ordered PTY write");
        };
        assert_eq!(bytes, expected);
    }
    assert!(completion.recv_timeout(Duration::from_secs(2)).unwrap());
    worker.join().unwrap();
}

#[test]
fn writer_shutdown_unblocks_a_backpressured_parser() {
    let (state, listener) = terminal_state(80, 24);
    let signal = Arc::new(FrameSignal::default());
    let (commands, incoming) = mpsc::sync_channel(1);
    commands.send(Command::Write(vec![1])).unwrap();
    let parser_signal = signal.clone();
    let (finished, completion) = mpsc::channel();
    let worker = thread::spawn(move || {
        let result = parse_output(
            &mut alacritty_terminal::vte::ansi::Processor::new(),
            &state,
            &listener,
            b"\x1b[5n",
            &commands,
            &parser_signal,
        );
        finished.send(result.is_err()).unwrap();
    });
    wait_until_dirty(&signal);
    drop(incoming);
    assert!(completion.recv_timeout(Duration::from_secs(2)).unwrap());
    worker.join().unwrap();
}

#[test]
fn close_flushes_pending_visual_state_without_waiting_for_the_normal_deadline() {
    let signal = Arc::new(FrameSignal::default());
    let publisher_signal = signal.clone();
    let (frames, incoming) = mpsc::channel();
    let worker = thread::spawn(move || {
        publisher_signal.publish(Duration::from_secs(3600), || {
            frames.send(()).unwrap();
        });
    });
    signal.request(false);
    incoming.recv_timeout(Duration::from_secs(2)).unwrap();
    signal.request(false);
    signal.close();
    incoming.recv_timeout(Duration::from_secs(2)).unwrap();
    worker.join().unwrap();
    assert!(incoming.try_recv().is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn real_pty_eof_publishes_the_final_short_burst_and_title() {
    let directory = tempfile::tempdir().unwrap();
    let (session, mut frames) = TerminalSession::spawn(directory.path()).unwrap();
    session
        .input(
            b"printf '\\033]2;Final burst\\007\\033[32mTAIL_COMPLETE\\033[0m\\n'; exit\n".to_vec(),
        )
        .unwrap();
    let final_frame = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            frames.changed().await.unwrap();
            let frame = frames.borrow_and_update().clone();
            if frame.exited {
                break frame;
            }
        }
    })
    .await
    .expect("PTY EOF must flush the final parsed state");
    assert_eq!(final_frame.title, "Final burst");
    assert!(
        final_frame
            .cells
            .iter()
            .any(|cell| cell.foreground == 0xa3be8c)
    );
    let text: String = final_frame
        .cells
        .iter()
        .map(|cell| cell.text.as_str())
        .collect();
    assert!(text.contains("TAIL_COMPLETE"));
}

#[cfg(unix)]
#[tokio::test]
async fn closing_a_saturated_real_pty_reaps_the_child_and_releases_every_worker() {
    let directory = tempfile::tempdir().unwrap();
    // Replace the interactive shell with a process which ignores graceful
    // hangup and never consumes stdin. Exec preserves the original,
    // supervisor-owned child identity.
    std::fs::write(
        directory.path().join("blocked-input.sh"),
        "trap '' HUP\nstty -echo -icanon min 1 time 0\nprintf '%s\\n' \"$$\" > owned-child.pid\nprintf '\\033]2;BLOCKED_INPUT_READY\\007'\nexec /bin/sleep 600\n",
    )
    .unwrap();
    let (session, mut frames) = TerminalSession::spawn(directory.path()).unwrap();
    let lifecycle = session.control.lifecycle.clone();
    session
        .input(b"exec /bin/sh blocked-input.sh\r".to_vec())
        .unwrap();
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            frames.changed().await.unwrap();
            let frame = frames.borrow_and_update();
            assert!(!frame.exited, "Fixture stopped before input saturation");
            if frame.title == "BLOCKED_INPUT_READY" {
                break;
            }
        }
    })
    .await
    .expect("Real PTY fixture must signal that it no longer reads stdin");
    let pid: libc::pid_t = std::fs::read_to_string(directory.path().join("owned-child.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    session.input(vec![b'x'; 2 * 1024 * 1024]).unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !lifecycle.writing.load(Ordering::Acquire) {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("The real PTY writer must enter write_all");
    // The write-in-progress hook plus a non-reading real PTY proves this is an
    // in-flight OS write, not merely a queued command.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(lifecycle.writing.load(Ordering::Acquire));
    let mut queued = 0;
    while session.input(vec![b'y']).is_ok() {
        queued += 1;
        assert!(queued <= 64, "Blocked writer unexpectedly consumed input");
    }
    assert_eq!(queued, 64, "Saturate the actual bounded command queue");
    assert!(!lifecycle.reaped.load(Ordering::Acquire));
    drop(session);
    tokio::time::timeout(Duration::from_secs(5), async {
        while ![
            &lifecycle.reaped,
            &lifecycle.reader_done,
            &lifecycle.control_done,
            &lifecycle.frames_done,
        ]
        .iter()
        .all(|complete| complete.load(Ordering::Acquire))
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("Closing must reap the child and release blocked I/O and frame workers");
    // This query is evidence only. Termination uses the retained Child object,
    // never a PID lookup as execution authority.
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
    while frames.has_changed().is_ok() {
        frames.borrow_and_update();
        if frames.changed().await.is_err() {
            break;
        }
    }
    eprintln!(
        "Real PTY close regression passed: 2 MiB write blocked, 64 queued inputs, ignored SIGHUP escalated, owned child reaped, reader/control/frame workers released. Unix owned-child scope; no detached descendant or Windows qualification."
    );
}

/// Explicit local tool qualification. Missing Vim is an error when requested,
/// never a passing skip. This tests PTY/VT behavior, not GPUI IME or mouse input.
#[cfg(unix)]
#[tokio::test]
#[ignore = "Requires /usr/bin/vim; opt-in real PTY/Unicode/resize/save smoke"]
async fn real_vim_unicode_resize_save_and_shell_resume() {
    async fn wait_frame(
        frames: &mut watch::Receiver<TerminalSnapshot>,
        phase: &str,
        predicate: impl Fn(&TerminalSnapshot) -> bool,
    ) -> TerminalSnapshot {
        tokio::time::timeout(Duration::from_secs(12), async {
            loop {
                frames.changed().await.expect("PTY frame stream stopped");
                let frame = frames.borrow_and_update().clone();
                assert!(
                    !frame.exited,
                    "Shell exited before {phase}: {:?}",
                    frame.error
                );
                if predicate(&frame) {
                    return frame;
                }
            }
        })
        .await
        .unwrap_or_else(|_| {
            let frame = frames.borrow();
            let tail: String = visible_text(&frame)
                .chars()
                .rev()
                .take(2048)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            panic!(
                "Real Vim smoke timed out during {phase}; title={:?}, exited={}, error={:?}, tail={tail:?}",
                frame.title, frame.exited, frame.error
            );
        })
    }
    fn visible_text(frame: &TerminalSnapshot) -> String {
        frame
            .cells
            .iter()
            .filter(|cell| !cell.wide_spacer)
            .map(|cell| cell.text.as_str())
            .collect()
    }

    let vim = Path::new("/usr/bin/vim");
    assert!(
        vim.is_file(),
        "This explicit smoke requires /usr/bin/vim; install nothing automatically"
    );
    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("unicode.txt");
    let (session, mut frames) = TerminalSession::spawn(directory.path()).unwrap();
    session
        .input(b"/usr/bin/vim -Nu NONE -n -i NONE -c 'set encoding=utf-8 fileencoding=utf-8 nobomb fileformat=unix' -c 'set title titlestring=XYLO_VIM_SMOKE' unicode.txt\r".to_vec())
        .unwrap();
    wait_frame(&mut frames, "Vim full-screen startup", |frame| {
        frame.title == "XYLO_VIM_SMOKE" && visible_text(frame).contains('~')
    })
    .await;

    let first_line = "UNICODE_VIM_BODY: 界 e\u{301} 🦀";
    let second_line = "Second line: café";
    session
        .input(format!("i{first_line}\r{second_line}\x1b").into_bytes())
        .unwrap();
    wait_frame(&mut frames, "Unicode insert", |frame| {
        let text = visible_text(frame);
        text.contains(first_line) && text.contains(second_line)
    })
    .await;
    session.resize(111, 31).unwrap();
    wait_frame(&mut frames, "resize while Vim owns the screen", |frame| {
        let text = visible_text(frame);
        frame.columns == 111
            && frame.rows == 31
            && frame.title == "XYLO_VIM_SMOKE"
            && text.contains(first_line)
            && text.contains(second_line)
            && text.contains('~')
    })
    .await;
    session
        .input(b"\x1b:echo 'XYLO_VIM_SIZE=' . &columns . 'x' . &lines\r".to_vec())
        .unwrap();
    wait_frame(
        &mut frames,
        "Vim observes the resized PTY dimensions",
        |frame| visible_text(frame).contains("XYLO_VIM_SIZE=111x31"),
    )
    .await;

    session.input(b"\x1b:wq\r".to_vec()).unwrap();
    let expected = format!("{first_line}\n{second_line}\n").into_bytes();
    tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            if std::fs::read(&target).ok().as_deref() == Some(expected.as_slice()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("Vim must save the exact UTF-8 file before quitting");
    wait_frame(
        &mut frames,
        "Vim restores the original terminal screen",
        |frame| {
            frame.title != "XYLO_VIM_SMOKE" && !visible_text(frame).contains("UNICODE_VIM_BODY")
        },
    )
    .await;

    // Octal encoding prevents a false pass from the terminal echoing the input
    // command itself. Only an actual resumed shell produces this plain marker.
    session
        .input(b"printf '\\130\\131\\114\\117\\137\\126\\111\\115\\137\\123\\110\\105\\114\\114\\137\\122\\105\\123\\125\\115\\105\\104\\n'\r".to_vec())
        .unwrap();
    let resumed = wait_frame(&mut frames, "shell resumes after Vim exits", |frame| {
        visible_text(frame).contains("XYLO_VIM_SHELL_RESUMED")
    })
    .await;
    assert_eq!(std::fs::read(&target).unwrap(), expected);
    assert!(
        !visible_text(&resumed).contains("UNICODE_VIM_BODY"),
        "Vim's alternate-screen contents must be removed after shell restoration"
    );
    assert!(resumed.error.is_none());
    session.input(b"exit\r".to_vec()).unwrap();
    eprintln!(
        "Real /usr/bin/vim smoke passed: UTF-8 CJK/combining/emoji file bytes, 111x31 resize, save+quit and actual shell marker. PTY/VT only; no GPUI IME/mouse, tmux or cross-platform qualification."
    );
}
