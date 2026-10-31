use super::{
    MAX_ASSIST_CONTEXT, SURFACE, TerminalElement, TerminalView, assistant_context, encode_key,
    terminal_run,
};
use gpui_kit::{App, Bounds, Element, Entity, Font, Window, point, px, size};
use xylo_runtime::terminal::TerminalCell;
use xylo_runtime::terminal::TerminalSnapshot;

#[test]
fn assistance_context_is_bounded_and_prefers_the_explicit_selection() {
    let mut snapshot = TerminalSnapshot {
        selection: Some(format!("{}尾\u{1b}", "界".repeat(MAX_ASSIST_CONTEXT))),
        ..Default::default()
    };
    let context = assistant_context(&snapshot);
    assert!(context.len() <= MAX_ASSIST_CONTEXT + "[Earlier output omitted]\n".len());
    assert!(context.ends_with('尾'));
    assert!(!context.contains('\u{1b}'));
    snapshot.selection = Some("user-selected output".into());
    assert_eq!(assistant_context(&snapshot), "user-selected output");
}

#[allow(dead_code)] // Invoked by the separate native renderer harness.
pub(crate) fn install_render_fixture(view: &mut TerminalView) {
    let columns = 110;
    let blank = TerminalCell {
        text: " ".into(),
        foreground: 0xe4e7ee,
        background: SURFACE,
        bold: false,
        italic: false,
        underline: false,
        wide_spacer: false,
        selected: false,
    };
    let mut cells = vec![blank; columns * 8];
    let prompt = "govind@Govinds-MacBook-Pro Documents % ";
    for (column, c) in prompt.chars().enumerate() {
        cells[column].text = c.to_string();
    }
    cells[columns].text = "界".into();
    cells[columns + 1].wide_spacer = true;
    cells[columns + 2].text = "e\u{301}".into();
    cells[columns + 3].text = "X".into();
    for cell in &mut cells[columns * 2..columns * 2 + 3] {
        cell.underline = true;
    }
    view.snapshot = TerminalSnapshot {
        columns,
        rows: 8,
        cells: std::sync::Arc::new(cells),
        cursor: Some((0, prompt.len())),
        title: "zsh".into(),
        ..Default::default()
    };
}

/// Uses the production prepaint path and the platform's actual shaper.
/// Mock text systems cannot detect a proportional fallback on macOS.
#[allow(dead_code)] // Invoked by the separate native renderer harness.
pub(crate) fn assert_render_geometry(
    view: Entity<TerminalView>,
    window: &mut Window,
    cx: &mut App,
) -> serde_json::Value {
    let mut element = TerminalElement { view: view.clone() };
    let bounds = Bounds::new(point(px(10.), px(42.)), size(px(940.), px(200.)));
    let lines = element.prepaint(None, None, bounds, &mut (), window, cx);
    let state = view.read(cx);
    let prompt = "govind@Govinds-MacBook-Pro Documents % ";
    let line = lines.iter().find(|line| line.row == 0).unwrap();
    let cursor_x = state.cell_width * prompt.len() as f32;
    let shaped_x = line.line.x_for_index(prompt.len());
    assert!(
        (cursor_x - shaped_x).abs() < px(1.),
        "cursor {cursor_x:?} separated from shaped prompt {shaped_x:?}"
    );
    let x = lines
        .iter()
        .find(|line| line.row == 1 && line.column == 3)
        .unwrap();
    assert!(
        x.line.text.starts_with('X'),
        "wide and combining cells shifted subsequent text"
    );
    assert!(
        lines.iter().any(|line| line.row == 2),
        "ANSI underlines on spaces must remain paintable"
    );
    let old_font = Font {
        family: "monospace".into(),
        ..Default::default()
    };
    let old_width = window
        .text_system()
        .shape_line(
            "M".into(),
            px(14.),
            &[terminal_run(&old_font, 1, 0xffffff, false, false, false)],
            None,
        )
        .width;
    let old_prompt = window.text_system().shape_line(
        prompt.into(),
        px(14.),
        &[terminal_run(
            &old_font,
            prompt.len(),
            0xffffff,
            false,
            false,
            false,
        )],
        None,
    );
    serde_json::json!({"font": state.font.family.to_string(), "font_size": state.font_size,
        "cursor_gap_pixels": f32::from(cursor_x - shaped_x),
        "old_generic_font_gap_pixels": f32::from(old_width * prompt.len() as f32 - old_prompt.width),
        "unicode_column_anchor": x.column})
}
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
