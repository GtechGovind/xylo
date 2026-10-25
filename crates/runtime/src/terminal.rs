//! Cross-platform PTY and Alacritty parser. All process, I/O, parser and search
//! work runs on dedicated threads; GPUI receives coalesced immutable snapshots.
use alacritty_terminal::{
    Term,
    event::{Event, EventListener},
    grid::{Dimensions, Scroll},
    index::{Column, Direction, Line, Point, Side},
    selection::{Selection, SelectionType},
    term::{Config, TermMode, cell::Flags, search::RegexSearch},
    vte::{
        ansi,
        ansi::{Color, NamedColor},
    },
};
use anyhow::{Result, anyhow};
use parking_lot::Mutex;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::{
    io::{Read, Write},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
};
use tokio::sync::watch;

#[derive(Clone, Debug)]
pub struct TerminalCell {
    pub text: String,
    pub foreground: u32,
    pub background: u32,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub wide_spacer: bool,
    pub selected: bool,
}
#[derive(Clone, Debug)]
pub struct TerminalSnapshot {
    pub columns: usize,
    pub rows: usize,
    pub cells: Arc<Vec<TerminalCell>>,
    pub cursor: Option<(usize, usize)>,
    pub display_offset: usize,
    pub bracketed_paste: bool,
    pub application_cursor: bool,
    pub title: String,
    pub selection: Option<String>,
    pub search_status: String,
    pub exited: bool,
    pub error: Option<String>,
}
impl Default for TerminalSnapshot {
    fn default() -> Self {
        Self {
            columns: 80,
            rows: 24,
            cells: Arc::new(vec![]),
            cursor: None,
            display_offset: 0,
            bracketed_paste: false,
            application_cursor: false,
            title: "Terminal".into(),
            selection: None,
            search_status: String::new(),
            exited: false,
            error: None,
        }
    }
}
#[derive(Clone, Copy)]
struct Size {
    columns: usize,
    rows: usize,
}
impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.columns
    }
}
enum Command {
    Write(Vec<u8>),
    Resize(u16, u16),
    Scroll(i32),
    Select {
        row: usize,
        column: usize,
        start: bool,
    },
    Search(String, bool),
    Title(String),
    Stop,
}
#[derive(Clone)]
struct Listener(mpsc::SyncSender<Command>);
impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        let command = match event {
            Event::PtyWrite(text) => Some(Command::Write(text.into_bytes())),
            Event::Title(title) => Some(Command::Title(title.chars().take(160).collect())),
            Event::ResetTitle => Some(Command::Title("Terminal".into())),
            _ => None,
        };
        if let Some(command) = command {
            let _ = self.0.try_send(command);
        }
    }
}
struct State {
    term: Term<Listener>,
    title: String,
    search_status: String,
    exited: bool,
    error: Option<String>,
}
struct Control {
    tx: mpsc::SyncSender<Command>,
    stop: Arc<AtomicBool>,
}
impl Drop for Control {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.tx.try_send(Command::Stop);
    }
}
#[derive(Clone)]
pub struct TerminalSession {
    control: Arc<Control>,
}
impl TerminalSession {
    /// Call from a background executor: spawning a shell can block.
    pub fn spawn(cwd: &Path) -> Result<(Self, watch::Receiver<TerminalSnapshot>)> {
        let pair = native_pty_system().openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let shell = if cfg!(windows) {
            std::env::var("COMSPEC").unwrap_or_else(|_| "powershell.exe".into())
        } else {
            std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
        };
        let mut command = CommandBuilder::new(shell);
        command.cwd(cwd);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        let mut child = pair.slave.spawn_command(command)?;
        drop(pair.slave);
        let mut killer = child.clone_killer();
        let mut reader = pair.master.try_clone_reader()?;
        let mut writer = pair.master.take_writer()?;
        let (tx, rx) = mpsc::sync_channel(64);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let state = Arc::new(Mutex::new(State {
            term: Term::new(
                Config::default(),
                &Size {
                    columns: 80,
                    rows: 24,
                },
                Listener(tx.clone()),
            ),
            title: "Terminal".into(),
            search_status: String::new(),
            exited: false,
            error: None,
        }));
        let (updates, receiver) = watch::channel(TerminalSnapshot::default());
        let parser_state = state.clone();
        let parser_updates = updates.clone();
        thread::Builder::new()
            .name("terminal-reader".into())
            .spawn(move || {
                let mut parser: alacritty_terminal::vte::ansi::Processor =
                    alacritty_terminal::vte::ansi::Processor::new();
                let mut buffer = [0; 8192];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(count) => {
                            let mut state = parser_state.lock();
                            parser.advance(&mut state.term, &buffer[..count]);
                            parser_updates.send_replace(snapshot(&state));
                        }
                        Err(error) => {
                            let mut state = parser_state.lock();
                            state.error = Some(error.to_string());
                            break;
                        }
                    }
                }
                let mut state = parser_state.lock();
                state.exited = true;
                parser_updates.send_replace(snapshot(&state));
            })?;
        thread::Builder::new()
            .name("terminal-control".into())
            .spawn(move || {
                while let Ok(command) = rx.recv() {
                    if worker_stop.load(Ordering::Acquire) {
                        break;
                    }
                    let result: Result<()> = match command {
                        Command::Write(bytes) => writer
                            .write_all(&bytes)
                            .and_then(|_| writer.flush())
                            .map_err(Into::into),
                        Command::Resize(columns, rows) => pair
                            .master
                            .resize(PtySize {
                                rows,
                                cols: columns,
                                pixel_width: 0,
                                pixel_height: 0,
                            })
                            .map(|_| {
                                state.lock().term.resize(Size {
                                    columns: columns as usize,
                                    rows: rows as usize,
                                })
                            }),
                        Command::Scroll(lines) => {
                            state.lock().term.scroll_display(Scroll::Delta(lines));
                            Ok(())
                        }
                        Command::Select { row, column, start } => {
                            let mut s = state.lock();
                            let offset = s.term.grid().display_offset();
                            let p = Point::new(
                                Line(row as i32 - offset as i32),
                                Column(column.min(s.term.columns() - 1)),
                            );
                            if start {
                                s.term.selection =
                                    Some(Selection::new(SelectionType::Simple, p, Side::Left));
                            } else if let Some(selection) = s.term.selection.as_mut() {
                                selection.update(p, Side::Right);
                            }
                            Ok(())
                        }
                        Command::Search(query, forward) => {
                            let mut s = state.lock();
                            if query.is_empty() {
                                s.search_status.clear();
                            } else {
                                match RegexSearch::new(&regex_escape(&query)) {
                                    Ok(mut regex) => {
                                        let origin = s
                                            .term
                                            .selection
                                            .as_ref()
                                            .and_then(|selection| selection.to_range(&s.term))
                                            .map(|r| if forward { r.end } else { r.start })
                                            .unwrap_or(s.term.grid().cursor.point);
                                        let dir = if forward {
                                            Direction::Right
                                        } else {
                                            Direction::Left
                                        };
                                        let origin = if forward {
                                            origin.add(
                                                &s.term,
                                                alacritty_terminal::index::Boundary::Grid,
                                                1,
                                            )
                                        } else {
                                            origin.sub(
                                                &s.term,
                                                alacritty_terminal::index::Boundary::Grid,
                                                1,
                                            )
                                        };
                                        if let Some(found) = s.term.search_next(
                                            &mut regex,
                                            origin,
                                            dir,
                                            Side::Left,
                                            None,
                                        ) {
                                            let mut selection = Selection::new(
                                                SelectionType::Simple,
                                                *found.start(),
                                                Side::Left,
                                            );
                                            selection.update(*found.end(), Side::Right);
                                            s.term.selection = Some(selection);
                                            s.term.scroll_to_point(*found.start());
                                            s.search_status = "Match found".into();
                                        } else {
                                            s.search_status = "No matches".into();
                                        }
                                    }
                                    Err(error) => s.search_status = error.to_string(),
                                }
                            }
                            Ok(())
                        }
                        Command::Title(title) => {
                            state.lock().title = title;
                            Ok(())
                        }
                        Command::Stop => {
                            let _ = killer.kill();
                            break;
                        }
                    };
                    let mut state = state.lock();
                    if let Err(error) = result {
                        state.error = Some(error.to_string());
                    }
                    updates.send_replace(snapshot(&state));
                }
                let _ = killer.kill();
                let _ = child.wait();
            })?;
        Ok((
            Self {
                control: Arc::new(Control { tx, stop }),
            },
            receiver,
        ))
    }
    fn command(&self, command: Command) -> Result<()> {
        self.control
            .tx
            .try_send(command)
            .map_err(|e| anyhow!("Terminal command queue: {e}"))
    }
    pub fn input(&self, bytes: impl Into<Vec<u8>>) -> Result<()> {
        self.command(Command::Write(bytes.into()))
    }
    pub fn paste(&self, text: &str, bracketed: bool) -> Result<()> {
        self.input(paste_bytes(text, bracketed))
    }
    pub fn resize(&self, columns: u16, rows: u16) -> Result<()> {
        self.command(Command::Resize(columns.clamp(2, 500), rows.clamp(1, 200)))
    }
    pub fn scroll(&self, lines: i32) -> Result<()> {
        self.command(Command::Scroll(lines))
    }
    pub fn select(&self, row: usize, column: usize, start: bool) -> Result<()> {
        self.command(Command::Select { row, column, start })
    }
    pub fn search(&self, query: String, forward: bool) -> Result<()> {
        self.command(Command::Search(query, forward))
    }
}
pub fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    let safe = text.replace('\u{1b}', "");
    if bracketed {
        format!("\x1b[200~{safe}\x1b[201~").into_bytes()
    } else {
        safe.replace("\r\n", "\n").replace('\n', "\r").into_bytes()
    }
}
fn regex_escape(s: &str) -> String {
    s.chars()
        .map(|c| {
            if "\\.^$|?*+()[]{}".contains(c) {
                format!("\\{c}")
            } else {
                c.to_string()
            }
        })
        .collect()
}
fn color(c: Color, background: bool) -> u32 {
    match c {
        Color::Spec(rgb) => ((rgb.r as u32) << 16) | ((rgb.g as u32) << 8) | rgb.b as u32,
        Color::Named(NamedColor::Foreground) => 0xdde2ee,
        Color::Named(NamedColor::Background) => 0x11151d,
        Color::Named(c) => indexed(c as usize),
        Color::Indexed(i) => {
            if i == 0 && background {
                0x11151d
            } else {
                indexed(i as usize)
            }
        }
    }
}
fn indexed(i: usize) -> u32 {
    const BASE: [u32; 16] = [
        0x1d2330, 0xf07178, 0xa3be8c, 0xebcb8b, 0x7aa2f7, 0xbb9af7, 0x7dcfff, 0xc0caf5, 0x565f89,
        0xff8990, 0xb9f27c, 0xffdc9a, 0x99b6ff, 0xd6b5ff, 0xa4e8ff, 0xffffff,
    ];
    if i < 16 {
        BASE[i]
    } else if i < 232 {
        let n = i - 16;
        let level = |v: usize| if v == 0 { 0 } else { 55 + v * 40 };
        ((level(n / 36) as u32) << 16) | ((level((n / 6) % 6) as u32) << 8) | level(n % 6) as u32
    } else {
        let v = (8 + (i.min(255) - 232) * 10) as u32;
        (v << 16) | (v << 8) | v
    }
}
fn snapshot(s: &State) -> TerminalSnapshot {
    let render = s.term.renderable_content();
    let columns = s.term.columns();
    let rows = s.term.screen_lines();
    let mut cells = Vec::with_capacity(columns * rows);
    for indexed in render.display_iter {
        let cell = indexed.cell;
        let mut fg = color(cell.fg, false);
        let mut bg = color(cell.bg, true);
        if cell.flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        let mut text = if cell.flags.contains(Flags::HIDDEN) {
            " ".into()
        } else {
            cell.c.to_string()
        };
        if let Some(zero) = cell.zerowidth() {
            text.extend(zero);
        }
        cells.push(TerminalCell {
            text,
            foreground: fg,
            background: bg,
            bold: cell.flags.contains(Flags::BOLD),
            italic: cell.flags.contains(Flags::ITALIC),
            underline: cell.flags.intersects(Flags::ALL_UNDERLINES),
            wide_spacer: cell.flags.contains(Flags::WIDE_CHAR_SPACER),
            selected: render.selection.is_some_and(|r| r.contains(indexed.point)),
        });
    }
    let row = render.cursor.point.line.0 + render.display_offset as i32;
    TerminalSnapshot {
        columns,
        rows,
        cells: Arc::new(cells),
        cursor: (row >= 0
            && (row as usize) < rows
            && render.cursor.shape != ansi::CursorShape::Hidden)
            .then_some((row.max(0) as usize, render.cursor.point.column.0)),
        display_offset: render.display_offset,
        bracketed_paste: render.mode.contains(TermMode::BRACKETED_PASTE),
        application_cursor: render.mode.contains(TermMode::APP_CURSOR),
        title: s.title.clone(),
        selection: s.term.selection_to_string(),
        search_status: s.search_status.clone(),
        exited: s.exited,
        error: s.error.clone(),
    }
}
#[cfg(test)]
mod tests {
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
        let (tx, _) = mpsc::sync_channel(8);
        let mut s = State {
            term: Term::new(
                Config::default(),
                &Size {
                    columns: 10,
                    rows: 2,
                },
                Listener(tx),
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
}
