//! Native terminal rendering. No webview: PTY parsing is in runtime::terminal,
//! glyph shaping, selection and IME are handled by GPUI.
use crate::ui::{border, button, muted, sidebar};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::{prelude::*, *};
use std::ops::Range;
use workbench_runtime::terminal::{TerminalSession, TerminalSnapshot};

pub struct TerminalView {
    session: TerminalSession,
    snapshot: TerminalSnapshot,
    focus: FocusHandle,
    bounds: Bounds<Pixels>,
    cell_width: Pixels,
    cell_height: Pixels,
    selecting: bool,
    preedit: String,
    search: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
    last_size: (u16, u16),
}
impl TerminalView {
    pub fn new(
        session: TerminalSession,
        mut updates: tokio::sync::watch::Receiver<TerminalSnapshot>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Find in scrollback"));
        let subscription = cx.subscribe_in(&search, window, |this, _, event, _, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.search_next(true, cx);
            }
        });
        cx.spawn(async move |this, cx| {
            while updates.changed().await.is_ok() {
                let snapshot = updates.borrow_and_update().clone();
                if this
                    .update(cx, |this, cx| {
                        this.snapshot = snapshot;
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        Self {
            session,
            snapshot: TerminalSnapshot::default(),
            focus: cx.focus_handle(),
            bounds: Bounds::default(),
            cell_width: px(8.4),
            cell_height: px(20.),
            selecting: false,
            preedit: String::new(),
            search,
            _subscriptions: vec![subscription],
            last_size: (80, 24),
        }
    }
    fn search_next(&mut self, forward: bool, cx: &mut Context<Self>) {
        if let Err(e) = self
            .session
            .search(self.search.read(cx).value().to_string(), forward)
        {
            self.snapshot.error = Some(e.to_string());
            cx.notify();
        }
    }
    fn send(&mut self, bytes: Vec<u8>, cx: &mut Context<Self>) {
        if let Err(e) = self.session.input(bytes) {
            self.snapshot.error = Some(e.to_string());
            cx.notify();
        }
    }
    fn copy(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = self.snapshot.selection.clone() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }
    fn paste(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|c| c.text())
            && let Err(e) = self.session.paste(&text, self.snapshot.bracketed_paste)
        {
            self.snapshot.error = Some(e.to_string());
            cx.notify();
        }
    }
    fn position(&self, point: Point<Pixels>) -> (usize, usize) {
        let row = ((point.y - self.bounds.top()) / self.cell_height)
            .floor()
            .max(0.) as usize;
        let col = ((point.x - self.bounds.left()) / self.cell_width)
            .floor()
            .max(0.) as usize;
        (
            row.min(self.snapshot.rows.saturating_sub(1)),
            col.min(self.snapshot.columns.saturating_sub(1)),
        )
    }
    fn key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let k = &event.keystroke;
        let clipboard_modifier = if cfg!(target_os = "macos") {
            k.modifiers.platform
        } else {
            k.modifiers.control && k.modifiers.shift
        };
        if clipboard_modifier && k.key == "c" {
            self.copy(cx);
            cx.stop_propagation();
            window.prevent_default();
            return;
        }
        if clipboard_modifier && k.key == "v" {
            self.paste(cx);
            cx.stop_propagation();
            window.prevent_default();
            return;
        }
        if clipboard_modifier && k.key == "f" {
            self.search.update(cx, |s, cx| s.focus(window, cx));
            cx.stop_propagation();
            window.prevent_default();
            return;
        }
        if let Some(bytes) = encode_key(
            &k.key,
            k.modifiers.control,
            k.modifiers.alt && !cfg!(target_os = "macos"),
            k.modifiers.shift,
            self.snapshot.application_cursor,
        ) {
            self.send(bytes, cx);
            cx.stop_propagation();
            window.prevent_default();
        }
    }
}
impl Focusable for TerminalView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
impl Render for TerminalView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .bg(rgb(0x11151d))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .h(px(40.))
                    .bg(rgb(sidebar()))
                    .border_b_1()
                    .border_color(rgb(border()))
                    .child(div().text_xs().text_color(rgb(muted())).child(
                        if self.snapshot.exited {
                            "Shell closed"
                        } else {
                            "Terminal"
                        },
                    ))
                    .child(div().flex_1())
                    .child(div().w(px(200.)).child(Input::new(&self.search)))
                    .child(
                        button("term-prev", "↑")
                            .on_click(cx.listener(|this, _, _, cx| this.search_next(false, cx))),
                    )
                    .child(
                        button("term-next", "↓")
                            .on_click(cx.listener(|this, _, _, cx| this.search_next(true, cx))),
                    )
                    .child(
                        button("term-copy", "Copy")
                            .on_click(cx.listener(|this, _, _, cx| this.copy(cx))),
                    )
                    .child(
                        button("term-paste", "Paste")
                            .on_click(cx.listener(|this, _, _, cx| this.paste(cx))),
                    ),
            )
            .child(
                div()
                    .id("terminal-surface")
                    .role(Role::Terminal)
                    .aria_label("Project terminal")
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .track_focus(&self.focus)
                    .key_context("WorkbenchTerminal")
                    .cursor(CursorStyle::IBeam)
                    .on_key_down(cx.listener(Self::key))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, window, cx| {
                            window.focus(&this.focus, cx);
                            this.selecting = true;
                            let (r, c) = this.position(event.position);
                            let _ = this.session.select(r, c, true);
                            cx.notify();
                        }),
                    )
                    .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, _| {
                        if this.selecting {
                            let (r, c) = this.position(event.position);
                            let _ = this.session.select(r, c, false);
                        }
                    }))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _, _, _| this.selecting = false),
                    )
                    .on_mouse_up_out(
                        MouseButton::Left,
                        cx.listener(|this, _, _, _| this.selecting = false),
                    )
                    .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                        let delta = event.delta.pixel_delta(this.cell_height).y;
                        let lines = (delta / this.cell_height).round() as i32;
                        if lines != 0 {
                            let _ = this.session.scroll(lines);
                            cx.stop_propagation();
                        }
                    }))
                    .child(TerminalElement { view: cx.entity() }),
            )
            .child(
                div()
                    .h(px(22.))
                    .bg(rgb(sidebar()))
                    .px_3()
                    .text_xs()
                    .text_color(rgb(muted()))
                    .child(self.snapshot.error.clone().unwrap_or_else(|| {
                        if !self.snapshot.search_status.is_empty() {
                            self.snapshot.search_status.clone()
                        } else {
                            if cfg!(target_os = "macos") {
                                "Ctrl+C to interrupt · ⌘C to copy · ⌘V to paste".into()
                            } else {
                                "Ctrl+C to interrupt · Ctrl+Shift+C to copy · Ctrl+Shift+V to paste"
                                    .into()
                            }
                        }
                    })),
            )
    }
}
/// Printable characters go through the native text input handler (including IME).
/// Only terminal control sequences are handled at key-down, preventing duplicates.
pub fn encode_key(
    key: &str,
    control: bool,
    alt: bool,
    shift: bool,
    application: bool,
) -> Option<Vec<u8>> {
    if control && key.len() == 1 {
        let c = key.as_bytes()[0].to_ascii_lowercase();
        if c.is_ascii_lowercase() {
            return Some(vec![c - b'a' + 1]);
        }
        if c == b'[' {
            return Some(vec![27]);
        }
        if c == b' ' {
            return Some(vec![0]);
        }
    }
    let s = match key {
        "enter" => "\r",
        "backspace" => "\x7f",
        "tab" if shift => "\x1b[Z",
        "tab" => "\t",
        "escape" => "\x1b",
        "up" if application => "\x1bOA",
        "down" if application => "\x1bOB",
        "right" if application => "\x1bOC",
        "left" if application => "\x1bOD",
        "up" => "\x1b[A",
        "down" => "\x1b[B",
        "right" => "\x1b[C",
        "left" => "\x1b[D",
        "home" => "\x1b[H",
        "end" => "\x1b[F",
        "delete" => "\x1b[3~",
        "pageup" => "\x1b[5~",
        "pagedown" => "\x1b[6~",
        "f1" => "\x1bOP",
        "f2" => "\x1bOQ",
        "f3" => "\x1bOR",
        "f4" => "\x1bOS",
        "f5" => "\x1b[15~",
        "f6" => "\x1b[17~",
        "f7" => "\x1b[18~",
        "f8" => "\x1b[19~",
        "f9" => "\x1b[20~",
        "f10" => "\x1b[21~",
        "f11" => "\x1b[23~",
        "f12" => "\x1b[24~",
        _ => {
            if alt && key.chars().count() == 1 {
                return Some(format!("\x1b{key}").into_bytes());
            }
            return None;
        }
    };
    Some(s.as_bytes().to_vec())
}
impl EntityInputHandler for TerminalView {
    fn text_for_range(
        &mut self,
        r: Range<usize>,
        actual: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let chars: Vec<u16> = self.preedit.encode_utf16().collect();
        let end = r.end.min(chars.len());
        let start = r.start.min(end);
        *actual = Some(start..end);
        Some(String::from_utf16_lossy(&chars[start..end]))
    }
    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let n = self.preedit.encode_utf16().count();
        Some(UTF16Selection {
            range: n..n,
            reversed: false,
        })
    }
    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        (!self.preedit.is_empty()).then(|| 0..self.preedit.encode_utf16().count())
    }
    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.preedit.clear();
        cx.notify();
    }
    fn replace_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.preedit.clear();
        self.send(text.as_bytes().to_vec(), cx);
        cx.notify();
    }
    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.preedit = text.into();
        cx.notify();
    }
    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let (row, col) = self.snapshot.cursor.unwrap_or((0, 0));
        Some(Bounds::new(
            self.bounds.origin + point(self.cell_width * col as f32, self.cell_height * row as f32),
            size(self.cell_width, self.cell_height),
        ))
    }
    fn character_index_for_point(
        &mut self,
        _: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        Some(0)
    }
}
struct TerminalElement {
    view: Entity<TerminalView>,
}
impl IntoElement for TerminalElement {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}
struct PaintedLine {
    line: ShapedLine,
    row: usize,
}
impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = Vec<PaintedLine>;
    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }
    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }
    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let font = Font {
            family: "monospace".into(),
            ..Default::default()
        };
        let measurement = window.text_system().shape_line(
            "M".into(),
            px(14.),
            &[TextRun {
                len: 1,
                font: font.clone(),
                color: rgb(0xdde2ee).into(),
                background_color: None,
                underline: None,
                strikethrough: None,
            }],
            None,
        );
        let width = measurement.width.max(px(1.));
        let height = px(20.);
        self.view.update(cx, |view, _| {
            view.bounds = bounds;
            view.cell_width = width;
            view.cell_height = height;
            let size = (
                (bounds.size.width / width).floor().clamp(2., 500.) as u16,
                (bounds.size.height / height).floor().clamp(1., 200.) as u16,
            );
            if size != view.last_size {
                let _ = view.session.resize(size.0, size.1);
                view.last_size = size;
            }
        });
        let view = self.view.read(cx);
        let mut lines = Vec::new();
        for (row, cells) in view
            .snapshot
            .cells
            .chunks(view.snapshot.columns.max(1))
            .enumerate()
        {
            let mut text = String::new();
            let mut runs: Vec<TextRun> = Vec::new();
            for cell in cells {
                if cell.wide_spacer {
                    continue;
                }
                text.push_str(&cell.text);
                let mut f = font.clone();
                if cell.bold {
                    f.weight = FontWeight::BOLD;
                }
                if cell.italic {
                    f.style = FontStyle::Italic;
                }
                let run = TextRun {
                    len: cell.text.len(),
                    font: f,
                    color: rgb(cell.foreground).into(),
                    background_color: None,
                    underline: cell.underline.then_some(UnderlineStyle {
                        color: Some(rgb(cell.foreground).into()),
                        thickness: px(1.),
                        wavy: false,
                    }),
                    strikethrough: None,
                };
                runs.push(run);
            }
            lines.push(PaintedLine {
                line: window
                    .text_system()
                    .shape_line(text.into(), px(14.), &runs, None),
                row,
            });
        }
        lines
    }
    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        lines: &mut Vec<PaintedLine>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let (cw, ch, focus, snapshot, preedit) = {
            let view = self.view.read(cx);
            (
                view.cell_width,
                view.cell_height,
                view.focus.clone(),
                view.snapshot.clone(),
                view.preedit.clone(),
            )
        };
        window.handle_input(
            &focus,
            ElementInputHandler::new(bounds, self.view.clone()),
            cx,
        );
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for (i, cell) in snapshot.cells.iter().enumerate() {
                let bg = if cell.selected {
                    0x354c67
                } else {
                    cell.background
                };
                if bg != 0x11151d {
                    window.paint_quad(fill(
                        Bounds::new(
                            bounds.origin
                                + point(
                                    cw * (i % snapshot.columns) as f32,
                                    ch * (i / snapshot.columns) as f32,
                                ),
                            size(cw, ch),
                        ),
                        rgb(bg),
                    ));
                }
            }
            for line in lines.iter() {
                let _ = line.line.paint(
                    bounds.origin + point(px(0.), ch * line.row as f32),
                    ch,
                    TextAlign::Left,
                    None,
                    window,
                    cx,
                );
            }
            if focus.is_focused(window)
                && let Some((row, col)) = snapshot.cursor
            {
                let origin = bounds.origin + point(cw * col as f32, ch * row as f32);
                window.paint_quad(fill(Bounds::new(origin, size(px(2.), ch)), rgb(0xa7edb2)));
                if !preedit.is_empty() {
                    let font = Font {
                        family: "monospace".into(),
                        ..Default::default()
                    };
                    let line = window.text_system().shape_line(
                        preedit.clone().into(),
                        px(14.),
                        &[TextRun {
                            len: preedit.len(),
                            font,
                            color: rgb(0xffffff).into(),
                            background_color: None,
                            underline: Some(UnderlineStyle {
                                color: None,
                                thickness: px(1.),
                                wavy: false,
                            }),
                            strikethrough: None,
                        }],
                        None,
                    );
                    let _ = line.paint(origin, ch, TextAlign::Left, None, window, cx);
                }
            }
        });
    }
}
#[cfg(test)]
mod tests {
    use super::encode_key;
    #[test]
    fn printable_keys_wait_for_native_ime() {
        assert_eq!(encode_key("a", false, false, false, false), None);
        assert_eq!(encode_key("界", false, false, false, false), None);
    }
    #[test]
    fn terminal_control_keys_are_encoded() {
        assert_eq!(encode_key("c", true, false, false, false), Some(vec![3]));
        assert_eq!(
            encode_key("up", false, false, false, true),
            Some(b"\x1bOA".to_vec())
        );
        assert_eq!(
            encode_key("tab", false, false, true, false),
            Some(b"\x1b[Z".to_vec())
        );
    }
}
