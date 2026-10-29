//! Native terminal geometry regression using the platform font shaper and GPU.
//! Output is a controlled rendering fixture, not a live command/session capture.
#![allow(dead_code, unused_imports)]
extern crate gpui_kit as gpui;
#[path = "../src/terminal_view.rs"]
mod terminal_view;
#[path = "../src/terminal_workspace.rs"]
mod terminal_workspace;
#[path = "../src/ui.rs"]
mod ui;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{prelude::*, *};
fn main() {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    run();
}
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn run() {
    use gpui_kit::component::{Theme, ThemeMode};
    let mut cx = HeadlessAppContext::with_platform(
        platform::current_platform(true).text_system(),
        std::sync::Arc::new(gpui_kit::assets::Assets),
        platform::current_headless_renderer,
    );
    cx.update(|cx| {
        gpui_kit::init(cx);
        Theme::change(ThemeMode::Dark, None, cx);
        ui::set_dark(true);
    });
    let cwd = tempfile::tempdir().unwrap();
    let (session, _live_updates) =
        xylo_runtime::terminal::TerminalSession::spawn(cwd.path()).unwrap();
    let (_fixture_sender, fixture_updates) = tokio::sync::watch::channel(Default::default());
    let (handle, view) = cx
        .update(|cx| {
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                        Default::default(),
                        size(px(960.), px(300.)),
                    ))),
                    show: false,
                    ..Default::default()
                },
                cx,
                |window, cx| {
                    cx.new(|cx| {
                        let view = terminal_workspace::TerminalWorkspace::new(
                            cwd.path().to_path_buf(),
                            session,
                            fixture_updates,
                            window,
                            cx,
                        );
                        view.active_view().unwrap().update(cx, |terminal, _| {
                            terminal_view::tests::install_render_fixture(terminal)
                        });
                        view
                    })
                },
            )
        })
        .unwrap();
    let proof = cx
        .update_window(handle, |_, window, cx| {
            view.update(cx, |view, cx| view.focus_active(window, cx));
            window.render_frame(cx);
            let terminal = view.read(cx).active_view().unwrap();
            terminal_view::tests::assert_render_geometry(terminal, window, cx)
        })
        .unwrap();
    let output = std::env::var_os("XYLO_RENDER_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| "target/native-terminal-preview".into());
    std::fs::create_dir_all(&output).unwrap();
    let capture = cx
        .capture_screenshot(handle)
        .expect("native terminal renderer available");
    for x in [0, capture.width() - 1] {
        assert_eq!(
            &capture.get_pixel(x, capture.height() / 2).0[..3],
            &[0x11, 0x15, 0x1d],
            "Single terminal pane must not have a bright focus outline"
        );
    }
    capture.save(output.join("terminal-fixture.png")).unwrap();
    cx.update_window(handle, |_, window, cx| {
        window.click("term-assist", cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.capture_screenshot(handle)
        .unwrap()
        .save(output.join("terminal-assist-fixture.png"))
        .unwrap();
    std::fs::write(
        output.join("geometry.json"),
        serde_json::to_vec_pretty(&proof).unwrap(),
    )
    .unwrap();
    println!(
        "Native terminal rendering fixture: {}\n{proof}",
        output.display()
    );
}
