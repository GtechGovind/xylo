//! App-level shortcuts bubble from native text controls; ordinary typing and IME
//! remain owned by GPUI Kit's input handlers.
use super::*;
actions!(
    workbench_ui,
    [
        SaveCurrentFile,
        NewConversation,
        FindConversation,
        FindProject,
        ToggleProjectTerminal
    ]
);
pub(super) fn bind(cx: &mut App) {
    let command = if cfg!(target_os = "macos") {
        "cmd"
    } else {
        "ctrl"
    };
    cx.bind_keys([
        KeyBinding::new(
            "ctrl-space",
            gpui_kit::component::input::ShowCompletions,
            Some("Input && mode == editor"),
        ),
        KeyBinding::new(&format!("{command}-s"), SaveCurrentFile, Some("Workbench")),
        KeyBinding::new(&format!("{command}-n"), NewConversation, Some("Workbench")),
        KeyBinding::new(&format!("{command}-k"), FindConversation, Some("Workbench")),
        KeyBinding::new(
            &format!("{command}-shift-f"),
            FindProject,
            Some("Workbench"),
        ),
        KeyBinding::new(
            &format!("{command}-j"),
            ToggleProjectTerminal,
            Some("Workbench"),
        ),
    ]);
}
