use super::*;
#[test]
fn bracketed_paste_cannot_inject_end_marker() {
    assert_eq!(
        paste_bytes("a\x1b[201~b界", true),
        b"\x1b[200~a[201~b\xe7\x95\x8c\x1b[201~"
    );
}
#[test]
fn paste_normalizes_line_endings() {
    assert_eq!(paste_bytes("a\r\nb\nc", false), b"a\rb\rc");
}
#[test]
fn parser_handles_colors_wide_characters_and_history() {
    let mut s = State {
        term: Term::new(
            Config::default(),
            &Size {
                columns: 10,
                rows: 2,
            },
            Listener::default(),
        ),
        title: String::new(),
        search_status: String::new(),
        exited: false,
        error: None,
    };
    let mut p: alacritty_terminal::vte::ansi::Processor =
        alacritty_terminal::vte::ansi::Processor::new();
    p.advance(&mut s.term, b"\x1b[31mRED\x1b[0m\r\n");
    p.advance(&mut s.term, "界e\u{301}".as_bytes());
    let frame = snapshot(&s);
    assert_eq!(frame.cells[0].foreground, 0xf07178);
    assert!(frame.cells.iter().any(|c| c.text.contains('界')));
    assert!(frame.cells.iter().any(|c| c.wide_spacer));
    assert!(frame.cells.iter().any(|c| c.text.contains('\u{301}')));
    p.advance(&mut s.term, b"\r\nnext\r\nlast");
    assert!(s.term.grid().history_size() > 0);
}
#[cfg(unix)]
#[tokio::test]
async fn real_pty_round_trips_output_resize_and_search() {
    let (session, mut frames) = TerminalSession::spawn(&std::env::temp_dir()).unwrap();
    session
        .input(b"printf '\\120\\124\\131\\137\\117\\113\\n'\n".to_vec())
        .unwrap();
    let frame = tokio::time::timeout(std::time::Duration::from_secs(8), async {
        loop {
            frames.changed().await.unwrap();
            let frame = frames.borrow_and_update().clone();
            let text: String = frame.cells.iter().map(|c| c.text.as_str()).collect();
            if text.contains("PTY_OK") {
                break frame;
            }
        }
    })
    .await
    .expect("shell produced actual printf output");
    assert!(frame.error.is_none());
    session.resize(111, 31).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            frames.changed().await.unwrap();
            if frames.borrow_and_update().columns == 111 {
                break;
            }
        }
    })
    .await
    .unwrap();
    session.search("PTY_OK".into(), true).unwrap();
    let found = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            frames.changed().await.unwrap();
            let frame = frames.borrow_and_update().clone();
            if frame.search_status == "Match found" {
                break frame;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(found.selection.as_deref(), Some("PTY_OK"));
}
