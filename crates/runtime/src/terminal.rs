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
use parking_lot::{Condvar, Mutex};
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
    time::{Duration, Instant},
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
    Stop,
}
#[derive(Default)]
struct ParserEvents {
    replies: Vec<String>,
    title: Option<String>,
}
#[derive(Clone, Default)]
struct Listener(Arc<Mutex<ParserEvents>>);
impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        // Parsing holds the terminal lock. Collect replies for this bounded
        // input packet and enqueue them only after releasing that lock: a full
        // writer queue must neither lose replies nor deadlock its consumer.
        match event {
            Event::PtyWrite(text) => self.0.lock().replies.push(text),
            Event::Title(title) => self.0.lock().title = Some(title.chars().take(160).collect()),
            Event::ResetTitle => self.0.lock().title = Some("Terminal".into()),
            _ => {}
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

const FRAME_INTERVAL: Duration = Duration::from_millis(16);

#[derive(Default)]
struct PendingFrame {
    dirty: bool,
    urgent: bool,
    closing: bool,
}

/// Coalesces requests before constructing a viewport, rather than merely
/// replacing already allocated frames in the watch channel. An idle session
/// waits on the condition variable without a polling timer.
#[derive(Default)]
struct FrameSignal {
    pending: Mutex<PendingFrame>,
    changed: Condvar,
}
impl FrameSignal {
    fn request(&self, urgent: bool) {
        let mut pending = self.pending.lock();
        let wake = !pending.dirty || urgent && !pending.urgent;
        pending.dirty = true;
        pending.urgent |= urgent;
        if wake {
            self.changed.notify_one();
        }
    }
    fn close(&self) {
        let mut pending = self.pending.lock();
        pending.closing = true;
        self.changed.notify_one();
    }
    fn publish(&self, interval: Duration, mut capture: impl FnMut()) {
        let mut previous = None::<Instant>;
        loop {
            let mut pending = self.pending.lock();
            loop {
                if pending.closing && !pending.dirty {
                    return;
                }
                if !pending.dirty {
                    self.changed.wait(&mut pending);
                    continue;
                }
                let remaining = previous.and_then(|time| interval.checked_sub(time.elapsed()));
                if !pending.urgent
                    && !pending.closing
                    && let Some(remaining) = remaining
                {
                    self.changed.wait_for(&mut pending, remaining);
                    continue;
                }
                pending.dirty = false;
                pending.urgent = false;
                break;
            }
            let closing = pending.closing;
            // No signal lock is held while taking the terminal lock or while
            // publishing. Producers can always mark the next frame dirty.
            drop(pending);
            capture();
            previous = Some(Instant::now());
            if closing {
                return;
            }
        }
    }
}

fn parse_output(
    parser: &mut alacritty_terminal::vte::ansi::Processor,
    state: &Mutex<State>,
    listener: &Listener,
    bytes: &[u8],
    commands: &mpsc::SyncSender<Command>,
    frames: &FrameSignal,
) -> Result<()> {
    let events = {
        let mut state = state.lock();
        parser.advance(&mut state.term, bytes);
        let events = std::mem::take(&mut *listener.0.lock());
        if let Some(title) = &events.title {
            state.title.clone_from(title);
        }
        events
    };
    frames.request(false);
    for reply in events.replies {
        commands
            .send(Command::Write(reply.into_bytes()))
            .map_err(|_| anyhow!("Terminal writer stopped"))?;
    }
    Ok(())
}

/// Unix PTY master writes can remain blocked after the slave child exits.
/// All clones share nonblocking mode; readiness waits also observe an owned
/// shutdown socket, so closing a tab releases both I/O workers directly.
#[cfg(unix)]
#[derive(Clone)]
struct UnixPtyIo {
    master: Arc<std::os::fd::OwnedFd>,
    shutdown: Arc<std::os::unix::net::UnixStream>,
    stop: Arc<AtomicBool>,
}
#[cfg(unix)]
impl UnixPtyIo {
    fn prepare(
        master: &dyn portable_pty::MasterPty,
        stop: Arc<AtomicBool>,
    ) -> Result<(Self, std::os::unix::net::UnixStream)> {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        let source = master
            .as_raw_fd()
            .ok_or_else(|| anyhow!("Terminal master has no native descriptor"))?;
        let duplicate = unsafe { libc::fcntl(source, libc::F_DUPFD_CLOEXEC, 0) };
        if duplicate == -1 {
            return Err(std::io::Error::last_os_error().into());
        }
        // Retain an owned descriptor throughout real I/O. A worker closing the
        // original master must not allow descriptor reuse during another poll.
        let master = unsafe { OwnedFd::from_raw_fd(duplicate) };
        let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
        if flags == -1
            || unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) }
                == -1
        {
            return Err(std::io::Error::last_os_error().into());
        }
        let (incoming, outgoing) = std::os::unix::net::UnixStream::pair()?;
        Ok((
            Self {
                master: Arc::new(master),
                shutdown: Arc::new(incoming),
                stop,
            },
            outgoing,
        ))
    }

    fn wait(&self, events: libc::c_short) -> std::io::Result<()> {
        use std::os::fd::AsRawFd;
        loop {
            if self.stop.load(Ordering::Acquire) {
                return Err(std::io::Error::from(std::io::ErrorKind::Interrupted));
            }
            let mut descriptors = [
                libc::pollfd {
                    fd: self.master.as_raw_fd(),
                    events,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.shutdown.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            let result = unsafe {
                libc::poll(
                    descriptors.as_mut_ptr(),
                    descriptors.len() as libc::nfds_t,
                    -1,
                )
            };
            if result == -1 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if self.stop.load(Ordering::Acquire) || descriptors[1].revents != 0 {
                return Err(std::io::Error::from(std::io::ErrorKind::Interrupted));
            }
            let ready = descriptors[0].revents;
            if ready & libc::POLLNVAL != 0 {
                return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
            }
            if events == libc::POLLOUT && ready & (libc::POLLHUP | libc::POLLERR) != 0 {
                return Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe));
            }
            if ready & (events | libc::POLLHUP | libc::POLLERR) != 0 {
                return Ok(());
            }
        }
    }

    fn read(&self, reader: &mut dyn Read, bytes: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if self.stop.load(Ordering::Acquire) {
                return Ok(0);
            }
            match reader.read(bytes) {
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if let Err(error) = self.wait(libc::POLLIN) {
                        if self.stop.load(Ordering::Acquire) {
                            return Ok(0);
                        }
                        return Err(error);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                result => return result,
            }
        }
    }

    fn write_all(&self, writer: &mut dyn Write, mut bytes: &[u8]) -> std::io::Result<()> {
        while !bytes.is_empty() {
            if self.stop.load(Ordering::Acquire) {
                return Err(std::io::Error::from(std::io::ErrorKind::Interrupted));
            }
            match writer.write(&bytes[..bytes.len().min(8192)]) {
                Ok(0) => return Err(std::io::Error::from(std::io::ErrorKind::WriteZero)),
                Ok(count) => bytes = &bytes[count..],
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    self.wait(libc::POLLOUT)?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        writer.flush()
    }
}

struct Control {
    tx: mpsc::SyncSender<Command>,
    stop: Arc<AtomicBool>,
    frames: Arc<FrameSignal>,
    shutdown: mpsc::SyncSender<()>,
    #[cfg(unix)]
    io_shutdown: std::os::unix::net::UnixStream,
    #[cfg(all(test, unix))]
    lifecycle: Arc<LifecycleProbe>,
}
impl Drop for Control {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        #[cfg(unix)]
        let _ = self.io_shutdown.shutdown(std::net::Shutdown::Write);
        // This channel is independent of the input writer, which may be
        // blocked inside a PTY write. Closing a tab never waits for child I/O.
        let _ = self.shutdown.try_send(());
        let _ = self.tx.try_send(Command::Stop);
        self.frames.close();
    }
}

/// Owns termination and reaping even when startup fails before its supervisor
/// thread is created. `spawn` is a background-only API; successful sessions move
/// this guard onto the supervisor, never onto the UI thread.
struct OwnedChild {
    child: Box<dyn portable_pty::Child + Send + Sync>,
    #[cfg(all(test, unix))]
    lifecycle: Arc<LifecycleProbe>,
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            // Use the actual child, not clone_killer: the pinned Unix child
            // implementation escalates an ignored SIGHUP before waiting.
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        #[cfg(all(test, unix))]
        self.lifecycle.reaped.store(true, Ordering::Release);
    }
}

#[cfg(all(test, unix))]
#[derive(Default)]
struct LifecycleProbe {
    owned_pid: std::sync::atomic::AtomicU32,
    writing: AtomicBool,
    reader_done: AtomicBool,
    control_done: AtomicBool,
    frames_done: AtomicBool,
    reaped: AtomicBool,
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
        #[cfg(all(test, unix))]
        let lifecycle = Arc::new(LifecycleProbe::default());
        let child = OwnedChild {
            child: pair.slave.spawn_command(command)?,
            #[cfg(all(test, unix))]
            lifecycle: lifecycle.clone(),
        };
        #[cfg(all(test, unix))]
        lifecycle
            .owned_pid
            .store(child.child.process_id().unwrap_or(0), Ordering::Release);
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader()?;
        let mut writer = pair.master.take_writer()?;
        let stop = Arc::new(AtomicBool::new(false));
        #[cfg(unix)]
        let (io, io_shutdown) = UnixPtyIo::prepare(pair.master.as_ref(), stop.clone())?;
        let (tx, rx) = mpsc::sync_channel(64);
        // There are only two logical producers: final Control drop and reader
        // completion. Neither blocks, including if both signal simultaneously.
        let (shutdown, stopping) = mpsc::sync_channel(2);
        thread::Builder::new()
            .name("terminal-supervisor".into())
            .spawn(move || {
                let _ = stopping.recv();
                drop(child);
            })?;
        let worker_stop = stop.clone();
        let frames = Arc::new(FrameSignal::default());
        let control = Arc::new(Control {
            tx: tx.clone(),
            stop,
            frames: frames.clone(),
            shutdown: shutdown.clone(),
            #[cfg(unix)]
            io_shutdown,
            #[cfg(all(test, unix))]
            lifecycle: lifecycle.clone(),
        });
        let listener = Listener::default();
        let state = Arc::new(Mutex::new(State {
            term: Term::new(
                Config::default(),
                &Size {
                    columns: 80,
                    rows: 24,
                },
                listener.clone(),
            ),
            title: "Terminal".into(),
            search_status: String::new(),
            exited: false,
            error: None,
        }));
        let (updates, receiver) = watch::channel(TerminalSnapshot::default());
        let frame_state = state.clone();
        let frame_signal = frames.clone();
        #[cfg(all(test, unix))]
        let frame_lifecycle = lifecycle.clone();
        thread::Builder::new()
            .name("terminal-frames".into())
            .spawn(move || {
                frame_signal.publish(FRAME_INTERVAL, || {
                    let frame = snapshot(&frame_state.lock());
                    updates.send_replace(frame);
                });
                #[cfg(all(test, unix))]
                frame_lifecycle.frames_done.store(true, Ordering::Release);
            })?;
        let parser_state = state.clone();
        let parser_frames = frames.clone();
        #[cfg(all(test, unix))]
        let parser_lifecycle = lifecycle.clone();
        #[cfg(unix)]
        let reader_io = io.clone();
        thread::Builder::new()
            .name("terminal-reader".into())
            .spawn(move || {
                let mut parser: alacritty_terminal::vte::ansi::Processor =
                    alacritty_terminal::vte::ansi::Processor::new();
                let mut buffer = [0; 8192];
                loop {
                    #[cfg(unix)]
                    let result = reader_io.read(reader.as_mut(), &mut buffer);
                    #[cfg(not(unix))]
                    let result = reader.read(&mut buffer);
                    match result {
                        Ok(0) => break,
                        Ok(count) => {
                            if let Err(error) = parse_output(
                                &mut parser,
                                &parser_state,
                                &listener,
                                &buffer[..count],
                                &tx,
                                &parser_frames,
                            ) {
                                parser_state.lock().error = Some(error.to_string());
                                break;
                            }
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
                drop(state);
                // EOF and errors must expose the complete tail immediately,
                // including a burst which ended before its next display tick.
                parser_frames.request(true);
                let _ = shutdown.try_send(());
                #[cfg(all(test, unix))]
                parser_lifecycle.reader_done.store(true, Ordering::Release);
            })?;
        thread::Builder::new()
            .name("terminal-control".into())
            .spawn(move || {
                while let Ok(command) = rx.recv() {
                    if worker_stop.load(Ordering::Acquire) {
                        break;
                    }
                    let visual_change = !matches!(&command, Command::Write(_));
                    let result: Result<()> = match command {
                        Command::Write(bytes) => {
                            #[cfg(all(test, unix))]
                            lifecycle.writing.store(true, Ordering::Release);
                            #[cfg(unix)]
                            let result = io.write_all(writer.as_mut(), &bytes).map_err(Into::into);
                            #[cfg(not(unix))]
                            let result = writer
                                .write_all(&bytes)
                                .and_then(|_| writer.flush())
                                .map_err(Into::into);
                            #[cfg(all(test, unix))]
                            lifecycle.writing.store(false, Ordering::Release);
                            result
                        }
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
                        Command::Stop => break,
                    };
                    if worker_stop.load(Ordering::Acquire) {
                        break;
                    }
                    if let Err(error) = result {
                        state.lock().error = Some(error.to_string());
                        frames.request(true);
                    } else if visual_change {
                        frames.request(false);
                    }
                }
                // Dropping the receiver releases a parser blocked while
                // sending protocol replies into a saturated command queue.
                drop(rx);
                #[cfg(all(test, unix))]
                lifecycle.control_done.store(true, Ordering::Release);
            })?;
        Ok((Self { control }, receiver))
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
        Color::Named(NamedColor::Foreground) => 0xe4e7ee,
        Color::Named(NamedColor::Background) => 0x0f1117,
        Color::Named(c) => indexed(c as usize),
        Color::Indexed(i) => {
            if i == 0 && background {
                0x0f1117
            } else {
                indexed(i as usize)
            }
        }
    }
}
fn indexed(i: usize) -> u32 {
    const BASE: [u32; 16] = [
        0x1c2029, 0xf07178, 0xa3be8c, 0xebcb8b, 0x7aa2f7, 0xbb9af7, 0x7dcfff, 0xc0caf5, 0x6b7380,
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
#[path = "terminal/coalescing_tests.rs"]
mod coalescing_tests;
#[cfg(test)]
#[path = "terminal/measurement.rs"]
mod measurement;
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
}
