//! App-level shortcuts bubble from native text controls; ordinary typing and IME
//! remain owned by GPUI Kit's input handlers. Cmd-. (Ctrl-. elsewhere) is the
//! explicit stop. Bare Escape first closes what is open (an inline rename, the
//! chat search palette) and acts on the task only while the
//! composer or the transcript has focus; inputs, menus and popovers consume
//! their own Escape before it reaches the window.
use super::*;
actions!(
    xylo_ui,
    [
        SaveCurrentFile,
        NewConversation,
        FindConversation,
        FindProject,
        ToggleProjectTerminal,
        AllowPendingAction,
        DenyOrStop,
        EscapePressed,
        FocusComposer
    ]
);
/// Expands to the platform's primary-modifier chord as a `&'static str`
/// (`cmd-…` on macOS, `ctrl-…` elsewhere), as GPUI key bindings require.
macro_rules! chord {
    ($keys:literal) => {
        if cfg!(target_os = "macos") {
            concat!("cmd-", $keys)
        } else {
            concat!("ctrl-", $keys)
        }
    };
}

pub(super) fn bind(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new(
            "ctrl-space",
            gpui_kit::component::input::ShowCompletions,
            Some("Input && mode == editor"),
        ),
        KeyBinding::new(chord!("s"), SaveCurrentFile, Some("Xylo")),
        KeyBinding::new(chord!("n"), NewConversation, Some("Xylo")),
        KeyBinding::new(chord!("k"), FindConversation, Some("Xylo")),
        KeyBinding::new(chord!("shift-f"), FindProject, Some("Xylo")),
        KeyBinding::new(chord!("j"), ToggleProjectTerminal, Some("Xylo")),
        // Text inputs own plain Enter/Escape; these chords still reach the
        // xylo while the composer is focused.
        KeyBinding::new(chord!("shift-enter"), AllowPendingAction, Some("Xylo")),
        KeyBinding::new(chord!("."), DenyOrStop, Some("Xylo")),
        KeyBinding::new("escape", EscapePressed, Some("Xylo")),
        KeyBinding::new(chord!("l"), FocusComposer, Some("Xylo")),
    ]);
}

impl Xylo {
    /// Routes a bare Escape that no input, menu or popover consumed.
    pub(super) fn escape_pressed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.sidebar_state.renaming_focused(window, cx) {
            self.cancel_rename(cx);
        } else if self.history_search.is_open() {
            self.close_history_search(cx);
        } else if self.escape_targets_task(window, cx) {
            self.deny_or_stop(cx);
        } else {
            cx.propagate();
        }
    }

    /// Escape may deny the pending approval or stop the task only on the chat,
    /// with nothing layered above it, while the composer or transcript has focus.
    fn escape_targets_task(&self, window: &Window, cx: &App) -> bool {
        self.page == Page::Chat
            && !self.settings_open
            && !self.model_picker.open
            && !self.history_search.is_open()
            && (self
                .composer
                .read(cx)
                .focus_handle(cx)
                .contains_focused(window, cx)
                || self.transcript_focus.contains_focused(window, cx))
    }
}
