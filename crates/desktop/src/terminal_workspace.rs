//! A project-scoped collection of native terminals. Session creation stays off
//! the UI thread; shell text/rendering/IME remain owned by `TerminalView`.
use crate::{
    icons::{self, AppIcon},
    terminal_view::{TerminalEvent, TerminalView},
    ui::{SingleLine, XyloTooltip, border, muted, panel, selected, sidebar, text},
};
use gpui_kit::{
    component::menu::{DropdownMenu, PopupMenuItem},
    prelude::*,
    *,
};
use std::path::PathBuf;
use tokio::sync::watch;
use uuid::Uuid;
use xylo_runtime::terminal::{TerminalSession, TerminalSnapshot};

const MAX_SESSIONS: usize = 12;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalWorkspaceEvent {
    Empty,
    AssistRequested { prompt: String },
}

#[derive(Clone, Copy)]
enum Placement {
    Tab,
    Split,
}

#[derive(Clone, Copy)]
struct PendingSpawn {
    id: Uuid,
    placement: Placement,
    anchor: Option<Uuid>,
}

/// Pane positions stay stable when focus changes. Selecting a hidden tab replaces
/// only the focused pane; collapsing a split keeps the other shell alive as a tab.
#[derive(Default)]
struct PaneLayout {
    left: Option<Uuid>,
    right: Option<Uuid>,
    active: Option<Uuid>,
}
impl PaneLayout {
    fn select(&mut self, id: Uuid) {
        if self.left != Some(id) && self.right != Some(id) {
            if self.right.is_some() && self.active == self.right {
                self.right = Some(id);
            } else {
                self.left = Some(id);
            }
        }
        self.active = Some(id);
    }
    fn split(&mut self, id: Uuid) {
        self.left = self.active;
        self.right = Some(id);
        self.active = Some(id);
    }
    fn collapse(&mut self) {
        self.left = self.active;
        self.right = None;
    }
    fn remove(&mut self, id: Uuid, remaining: &[Uuid]) {
        if self.left == Some(id) {
            self.left = self.right.take().or_else(|| remaining.first().copied());
        } else if self.right == Some(id) {
            self.right = None;
        }
        if self.active == Some(id) || self.active.is_none() {
            self.active = self.left.or(self.right);
        }
    }
}

struct ShellTab {
    id: Uuid,
    title: String,
    exited: bool,
    view: Entity<TerminalView>,
    _subscriptions: Vec<Subscription>,
}

pub struct TerminalWorkspace {
    /// Starting directory only. A shell may change directory after creation.
    cwd: PathBuf,
    tabs: Vec<ShellTab>,
    layout: PaneLayout,
    scroll: ScrollHandle,
    next_number: usize,
    pending: Option<PendingSpawn>,
    confirming: Option<Uuid>,
    error: Option<String>,
    closed: bool,
}
impl EventEmitter<TerminalWorkspaceEvent> for TerminalWorkspace {}

impl TerminalWorkspace {
    pub fn new(
        cwd: PathBuf,
        session: TerminalSession,
        updates: watch::Receiver<TerminalSnapshot>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            cwd,
            tabs: Vec::new(),
            layout: PaneLayout::default(),
            scroll: ScrollHandle::new(),
            next_number: 1,
            pending: None,
            confirming: None,
            error: None,
            closed: false,
        };
        let id = Uuid::new_v4();
        this.add_shell(id, session, updates, window, cx);
        this.layout.select(id);
        this
    }

    /// How many shells exist, including exited ones. For validation.
    #[cfg(feature = "validation")]
    pub fn session_count(&self) -> usize {
        self.tabs.len()
    }

    /// Whether any shell is starting or still running. For validation.
    #[cfg(feature = "validation")]
    pub fn has_live_sessions(&self) -> bool {
        self.pending.is_some() || self.tabs.iter().any(|tab| !tab.exited)
    }

    pub fn active_view(&self) -> Option<Entity<TerminalView>> {
        self.active_tab().map(|tab| tab.view.clone())
    }

    /// A handle to the selected existing shell, never a new execution
    /// request. For validation.
    #[cfg(feature = "validation")]
    pub fn active_session(&self, cx: &App) -> Option<TerminalSession> {
        self.active_tab().map(|tab| tab.view.read(cx).session())
    }

    pub fn focus_active(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(view) = self.active_view() {
            view.update(cx, |view, cx| view.focus_terminal(window, cx));
        }
    }

    pub fn new_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.start_shell(Placement::Tab, window, cx);
    }

    pub fn split_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.layout.right.is_none() {
            self.start_shell(Placement::Split, window, cx);
        }
    }

    /// An explicit app-level close action. Xylo owns project/Quit/update
    /// confirmation. All outstanding start replies are fenced before dropping
    /// sessions, so a delayed spawn cannot recreate a closed workspace.
    pub fn close_all(&mut self, cx: &mut Context<Self>) {
        self.closed = true;
        self.pending = None;
        self.confirming = None;
        self.tabs.clear();
        self.layout = PaneLayout::default();
        cx.emit(TerminalWorkspaceEvent::Empty);
        cx.notify();
    }

    fn active_tab(&self) -> Option<&ShellTab> {
        self.tabs
            .iter()
            .find(|tab| Some(tab.id) == self.layout.active)
    }

    fn add_shell(
        &mut self,
        id: Uuid,
        session: TerminalSession,
        mut updates: watch::Receiver<TerminalSnapshot>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let initial = updates.borrow().clone();
        let view = cx.new(|cx| TerminalView::new(session, updates.clone(), window, cx));
        let assist = cx.subscribe(&view, move |this, _, event, cx| {
            if this.tabs.iter().any(|tab| tab.id == id) {
                match event {
                    TerminalEvent::AssistRequested { prompt } => {
                        cx.emit(TerminalWorkspaceEvent::AssistRequested {
                            prompt: prompt.clone(),
                        });
                    }
                }
            }
        });
        let focus = view.read(cx).focus_handle(cx);
        let focused = cx.on_focus_in(&focus, window, move |this, _, cx| {
            this.activate_visible(id, cx);
        });
        let number = self.next_number;
        self.next_number += 1;
        self.tabs.push(ShellTab {
            id,
            title: safe_title(&initial.title, number),
            exited: initial.exited,
            view,
            _subscriptions: vec![assist, focused],
        });
        cx.spawn(async move |this, cx| {
            while updates.changed().await.is_ok() {
                let (title, exited) = {
                    let snapshot = updates.borrow_and_update();
                    (safe_title(&snapshot.title, number), snapshot.exited)
                };
                let mut keep = false;
                if this
                    .update(cx, |this, cx| {
                        if let Some(tab) = this.tabs.iter_mut().find(|tab| tab.id == id) {
                            keep = true;
                            if tab.title != title || tab.exited != exited {
                                tab.title = title;
                                tab.exited = exited;
                                cx.notify();
                            }
                        }
                    })
                    .is_err()
                    || !keep
                {
                    break;
                }
            }
        })
        .detach();
    }

    fn start_shell(&mut self, placement: Placement, window: &mut Window, cx: &mut Context<Self>) {
        if self.closed || self.pending.is_some() || self.tabs.len() >= MAX_SESSIONS {
            return;
        }
        let request = PendingSpawn {
            id: Uuid::new_v4(),
            placement,
            anchor: self.layout.active,
        };
        self.pending = Some(request);
        self.error = None;
        let cwd = self.cwd.clone();
        let task = cx
            .background_executor()
            .spawn(async move { TerminalSession::spawn(&cwd) });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.closed
                    || this.pending.as_ref().map(|pending| pending.id) != Some(request.id)
                {
                    return;
                }
                this.pending = None;
                match result {
                    Ok((session, updates)) => {
                        this.add_shell(request.id, session, updates, window, cx);
                        // Do not steal focus after the user has selected another
                        // terminal while shell startup was still in flight.
                        if this.layout.active == request.anchor {
                            match request.placement {
                                Placement::Split if this.layout.right.is_none() => {
                                    this.layout.split(request.id)
                                }
                                _ => this.layout.select(request.id),
                            }
                            this.focus_active(window, cx);
                        }
                        this.scroll
                            .scroll_to_item(this.tabs.len().saturating_sub(1));
                    }
                    Err(error) => {
                        this.error = Some(format!(
                            "Couldn't start a terminal: {}",
                            error.to_string().chars().take(240).collect::<String>()
                        ));
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn activate_visible(&mut self, id: Uuid, cx: &mut Context<Self>) {
        if self.tabs.iter().any(|tab| tab.id == id) {
            self.layout.select(id);
            cx.notify();
        }
    }

    fn select(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        self.activate_visible(id, cx);
        if let Some(index) = self.tabs.iter().position(|tab| tab.id == id) {
            self.scroll.scroll_to_item(index);
        }
        self.focus_active(window, cx);
    }

    fn request_close(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        if self.confirming.is_some() {
            return;
        }
        let Some(tab) = self.tabs.iter().find(|tab| tab.id == id) else {
            return;
        };
        if tab.exited {
            self.remove(id, window, cx);
            return;
        }
        self.confirming = Some(id);
        let answer = window.prompt(
            PromptLevel::Warning,
            "Close this terminal?",
            Some("Closing this terminal stops its shell and removes its scrollback. Finish or stop running work first."),
            &[PromptButton::cancel("Keep terminal"), PromptButton::new("Close terminal")],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let answer = answer.await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.confirming == Some(id) {
                    this.confirming = None;
                    if matches!(answer, Ok(1)) {
                        this.remove(id, window, cx);
                    }
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn remove(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.tabs.iter().position(|tab| tab.id == id) else {
            return;
        };
        self.tabs.remove(index);
        let remaining: Vec<_> = self.tabs.iter().map(|tab| tab.id).collect();
        self.layout.remove(id, &remaining);
        if self.tabs.is_empty() {
            self.closed = true;
            self.pending = None;
            cx.emit(TerminalWorkspaceEvent::Empty);
        } else {
            self.focus_active(window, cx);
        }
        cx.notify();
    }

    /// Runs `action` on the focused shell's view, for the toolbar's menu.
    fn with_active_view(
        &self,
        cx: &mut Context<Self>,
        action: impl FnOnce(&mut TerminalView, &mut Context<TerminalView>),
    ) {
        if let Some(view) = self.active_view() {
            view.update(cx, action);
        }
    }

    /// The toolbar's "…" menu: actions on the focused shell that have
    /// keyboard shortcuts and so do not need their own buttons.
    fn more_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let weak = cx.entity().downgrade();
        icons::menu_trigger("terminal-more", AppIcon::More, "More terminal actions")
            .dropdown_menu(move |menu, _, _| {
                let item = |label: &'static str,
                            action: fn(
                    &mut TerminalView,
                    &mut Window,
                    &mut Context<TerminalView>,
                )| {
                    let weak = weak.clone();
                    PopupMenuItem::new(label).on_click(move |_, window, cx| {
                        let _ = weak.update(cx, |this, cx| {
                            this.with_active_view(cx, |view, cx| action(view, window, cx))
                        });
                    })
                };
                menu.item(item("Ask Xylo about output", |view, window, cx| {
                    view.toggle_assist(window, cx)
                }))
                .item(item("Find…", |view, window, cx| {
                    view.open_search(window, cx)
                }))
                .separator()
                .item(item("Bigger text", |view, _, cx| view.zoom(1., cx)))
                .item(item("Smaller text", |view, _, cx| view.zoom(-1., cx)))
                .item(item("Actual size", |view, _, cx| view.reset_zoom(cx)))
            })
            .into_any_element()
    }

    fn pane(&self, id: Uuid, cx: &mut Context<Self>) -> AnyElement {
        let Some(tab) = self.tabs.iter().find(|tab| tab.id == id) else {
            return div().into_any_element();
        };
        div()
            .id(element_id(id, "pane"))
            .flex_1()
            .min_w_0()
            .min_h_0()
            .h_full()
            .when(self.layout.right == Some(id), |pane| {
                pane.border_l_1().border_color(rgb(border()))
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| this.activate_visible(id, cx)),
            )
            .child(tab.view.clone())
            .into_any_element()
    }
}

impl Render for TerminalWorkspace {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let busy = self.pending.is_some();
        let full = self.tabs.len() >= MAX_SESSIONS;
        let mut strip = div()
            .id("terminal-tabs")
            .flex()
            .items_center()
            .gap_1()
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_x_scroll()
            .track_scroll(&self.scroll);
        // Where every shell started; shown on hover instead of as toolbar text.
        let started_in: SharedString = format!("Started in {}", self.cwd.display()).into();
        for tab in &self.tabs {
            let id = tab.id;
            let active = self.layout.active == Some(id);
            let label = if tab.exited {
                format!("{} · Closed", tab.title)
            } else {
                tab.title.clone()
            };
            let tooltip = started_in.clone();
            strip = strip.child(
                div()
                    .flex()
                    .items_center()
                    .flex_shrink_0()
                    .h(px(28.))
                    .pr(px(2.))
                    .rounded(px(7.))
                    .bg(rgb(if active { selected() } else { sidebar() }))
                    .child(
                        gpui_kit::base::Button::new(element_id(id, "tab"))
                            .accessibility_label(format!(
                                "Select {label}{}",
                                if active { " (active)" } else { "" }
                            ))
                            .tooltip(move |_, cx| cx.new(|_| XyloTooltip(tooltip.clone())).into())
                            .selected(active)
                            .flex()
                            .justify_start()
                            .items_center()
                            .gap(px(6.))
                            .h(px(28.))
                            .min_w(px(72.))
                            .max_w(px(200.))
                            .pl(px(8.))
                            .pr(px(4.))
                            .rounded(px(7.))
                            .text_size(px(12.))
                            .text_color(rgb(if active { text() } else { muted() }))
                            .hover(|style| style.text_color(rgb(text())))
                            .child(icons::small(AppIcon::Terminal))
                            .child(div().min_w_0().single_line().child(label))
                            .on_click(
                                cx.listener(move |this, _, window, cx| this.select(id, window, cx)),
                            ),
                    )
                    .child(
                        icons::button_frame(element_id(id, "close"), "Close terminal")
                            .size(px(22.))
                            .child(icons::small(AppIcon::Close))
                            .disabled(self.confirming.is_some())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.request_close(id, window, cx)
                            })),
                    ),
            );
        }
        let split = self.layout.right.is_some();
        let header = div()
            .flex()
            .items_center()
            .gap(px(2.))
            .h(px(40.))
            .flex_shrink_0()
            .px(px(8.))
            .bg(rgb(sidebar()))
            .border_b_1()
            .border_color(rgb(border()))
            .child(strip)
            .when(busy, |header| {
                header.child(
                    div()
                        .px(px(6.))
                        .child(crate::motion::spinner(icons::SMALL).color(rgb(muted()).into())),
                )
            })
            .child(
                icons::icon_button("terminal-new", AppIcon::Add, "New terminal")
                    .disabled(busy || full)
                    .on_click(cx.listener(|this, _, window, cx| this.new_tab(window, cx))),
            )
            // Split is a toggle: pressed while two panes show.
            .child(if split {
                icons::icon_button("terminal-unsplit", AppIcon::Split, "Show one pane")
                    .bg(rgb(selected()))
                    .text_color(rgb(text()))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.layout.collapse();
                        this.focus_active(window, cx);
                        cx.notify();
                    }))
            } else {
                icons::icon_button("terminal-split", AppIcon::Split, "Split terminal")
                    .disabled(busy || full)
                    .on_click(cx.listener(|this, _, window, cx| this.split_active(window, cx)))
            })
            .child(self.more_menu(cx));
        let mut body = div().flex().flex_1().min_h_0().min_w_0().bg(rgb(0x0f1117));
        if let Some(id) = self.layout.left {
            body = body.child(self.pane(id, cx));
        }
        if let Some(id) = self.layout.right {
            body = body.child(self.pane(id, cx));
        }
        let mut result = div()
            .id("terminal-workspace")
            .flex()
            .flex_col()
            .size_full()
            .min_h_0()
            .min_w_0()
            .bg(rgb(panel()))
            .child(header);
        if let Some(error) = &self.error {
            result = result.child(
                div()
                    .px_3()
                    .py_1()
                    .text_xs()
                    .text_color(rgb(crate::ui::danger()))
                    .child(error.clone()),
            );
        }
        result.child(body)
    }
}

fn safe_title(title: &str, number: usize) -> String {
    let title: String = title
        .chars()
        .filter(|c| {
            !c.is_control() && !matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .take(64)
        .collect();
    let title = title.trim();
    if title.is_empty() || title == "Terminal" {
        format!("Shell {number}")
    } else {
        title.into()
    }
}

fn element_id(id: Uuid, part: &'static str) -> ElementId {
    (ElementId::from(id), part).into()
}
