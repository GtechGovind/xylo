//! Controlled portable tool-card rendering; protocol-shaped outputs are fixtures.
//! Uses production GPUI cards, native fonts/GPU and a temporary Host; no tools execute.
//! No remote discovery, model inference, account credentials or physical desktop.
#![allow(dead_code, unused_imports)]
extern crate gpui_kit as gpui;
#[path = "../src/state.rs"]
mod state;
#[path = "../src/terminal_view.rs"]
mod terminal_view;
#[path = "../src/terminal_workspace.rs"]
mod terminal_workspace;
#[path = "../src/ui.rs"]
mod ui;
mod xylo {
    include!("../src/xylo.rs");
    impl Xylo {
        pub(crate) fn extension_review_theme(&self) -> &str {
            &self.settings.theme
        }
        pub(crate) fn portable_card(&mut self, name: &str, cx: &mut Context<Self>) {
            self.messages.clear();
            self.turns.clear();
            self.expanded_tools.clear();
            self.raw_tool_details.clear();
            let action = Uuid::new_v4();
            let turn = self.turns.entry(self.session).or_default();
            turn.apply(EngineEvent::Started);
            let (input, summary) = if name == "extension_skill" {
                (
                    serde_json::json!({"id":"extension-workspace-diagnostics:check-project"}),
                    serde_json::json!({"instructions":"## Check the project\n\nRead configuration before running diagnostics. Preserve user changes and report the evidence for each finding.","package_root":"/controlled/installed-package"}),
                )
            } else {
                (
                    serde_json::json!({"id":"extension-workspace-diagnostics","component":"mcpServers","operation":"tools/call"}),
                    serde_json::json!({"status":{"readiness":"execution_verified"},"results":[{"entry":"project_summary","output":{"content":[{"type":"text","text":"Three source files checked. No build errors found."}],"isError":false}},{"entry":"after_tools","output":{"stdout":"Saved a local diagnostic report.","stderr":"","exit_code":0}}]}),
                )
            };
            turn.apply(EngineEvent::ToolStarted {
                action_id: action,
                name: name.into(),
                arguments: input,
            });
            turn.apply(EngineEvent::ToolFinished {
                action_id: action,
                name: name.into(),
                result: serde_json::json!({"status":"success","summary":summary}).to_string(),
                success: true,
            });
            let shown = &turn.tools[0].presentation;
            if name == "extension_skill" {
                assert_eq!(shown.verb, "Use skill");
                assert_eq!(shown.target, "check-project");
            } else {
                assert_eq!(shown.verb, "Run plugin");
                assert_eq!(shown.target, "mcpServers");
                assert_eq!(shown.meta.as_deref(), Some("tools/call"));
            }
            self.invalidate_live_history();
            cx.notify();
        }
        pub(crate) fn portable_preview_actual_shapes(&self) {
            use crate::state::tool_presentation::ToolBody;
            let shown = &self.turns[&self.session].tools[0].presentation;
            if let ToolBody::Message(text) = &shown.body {
                assert!(
                    text.contains("Three source files checked."),
                    "Real MCP text content array must appear in semantic preview: {text}"
                );
                assert!(
                    text.contains("Saved a local diagnostic report."),
                    "Real hook stdout must appear in semantic preview: {text}"
                );
                assert!(!text.contains("\"content\""));
            }
        }
        pub(crate) fn portable_preview_bounded() {
            use crate::state::tool_presentation::{ToolBody, ToolPresentation};
            let skill =
                serde_json::json!({"summary":{"instructions":"Long instruction. ".repeat(2000)}})
                    .to_string();
            let shown = ToolPresentation::prepare(
                "extension_skill",
                &serde_json::json!({"id":"extension-fixture:review"}),
                Some(&skill),
                Some(true),
            );
            assert!(
                matches!(shown.body,ToolBody::Markdown(ref text) if text.chars().count()<=8001)
            );
            let result=serde_json::json!({"summary":{"results":(0..100).map(|i|serde_json::json!({"entry":format!("entry-{i}"),"output":{"text":"Long result. ".repeat(1000)}})).collect::<Vec<_>>()}}).to_string();
            let shown = ToolPresentation::prepare(
                "extension_component",
                &serde_json::json!({"component":"mcpServers","operation":"tools/call"}),
                Some(&result),
                Some(true),
            );
            assert!(
                matches!(shown.body,ToolBody::Message(ref text) if text.lines().count()<=24&&text.chars().count()<=13000)
            );
        }

        pub(crate) fn capture_ready(&self) -> bool {
            self.settings_loaded
                && self.workspace_ready
                && !self.file_browser.indexing
                && !self.file_browser.loading()
        }
        pub(crate) fn capture_healthy(&self) {
            assert!(self.error.is_none(), "Actual Host error: {:?}", self.error);
            assert!(!self.running(), "Capture must not invoke a model turn");
        }
    }
}
fn main() {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    render();
}
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn render() {
    use gpui_kit::{
        component::{Theme, ThemeMode},
        prelude::*,
        test::{TestQueryExt, TestWindowExt},
        *,
    };
    use std::{
        path::{Path, PathBuf},
        sync::Arc,
        time::{Duration, Instant},
    };
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("northstar");
    std::fs::create_dir_all(project.join("src")).unwrap();
    std::fs::write(
        project.join("README.md"),
        "# Northstar\n\nA focused desktop workspace.\n",
    )
    .unwrap();
    std::fs::write(
        project.join("src/main.rs"),
        "fn main() {\n    println!(\"Hello\");\n}\n",
    )
    .unwrap();
    let project = project.canonicalize().unwrap();
    let paths = xylo_runtime::AppPaths::at(temporary.path().join("profile")).unwrap();
    xylo_services::Store::open(&paths.database)
        .unwrap()
        .save_settings(&xylo_core::AppSettings {
            theme: "dark".into(),
            last_workspace: Some(project.clone()),
            ..Default::default()
        })
        .unwrap();
    let host = xylo_runtime::host::HostClient::spawn_at(paths).unwrap();
    let client = host.0.clone();
    let mut cx = HeadlessAppContext::with_platform(
        platform::current_platform(true).text_system(),
        Arc::new(gpui_kit::assets::Assets),
        platform::current_headless_renderer,
    );
    cx.allow_parking();
    cx.update(gpui_kit::init);
    let (handle, view) = cx
        .update(|cx| {
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                        Default::default(),
                        size(px(1380.), px(880.)),
                    ))),
                    focus: false,
                    show: false,
                    ..Default::default()
                },
                cx,
                |window, cx| cx.new(|cx| xylo::Xylo::new(Ok(host), window, cx)),
            )
        })
        .unwrap();
    let wait = |cx: &mut HeadlessAppContext, condition: &dyn Fn(&xylo::Xylo) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            cx.run_until_parked();
            if cx.update(|cx| condition(view.read(cx))) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Real Host capture did not become ready"
            );
            cx.advance_clock(Duration::from_millis(2));
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    wait(&mut cx, &|view| view.capture_ready());
    let output = std::env::var_os("XYLO_RENDER_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| "target/native-preview".into());
    std::fs::create_dir_all(&output).unwrap();
    xylo::Xylo::portable_preview_bounded();
    for theme in ["dark", "light"] {
        let mut settings = xylo_core::AppSettings::default();
        settings.theme = theme.into();
        settings.last_workspace = Some(project.clone());
        client.save_settings(settings).unwrap();
        wait(&mut cx, &|view| view.extension_review_theme() == theme);
        for width in [1000u32, 1380, 1920] {
            cx.update_window(handle, |_, window, cx| {
                window.resize(size(px(width as f32), px(880.)));
                window.bounds_changed(cx);
            })
            .unwrap();
            cx.run_until_parked();
            for name in ["extension_skill", "extension_component"] {
                cx.update_window(handle, |_, window, cx| {
                    view.update(cx, |v, cx| v.portable_card(name, cx));
                    window.render_frame(cx);
                    assert!(
                        window.try_find(("tool-raw-toggle", 0usize)).is_none(),
                        "Cards must start closed"
                    );
                    assert!(window.find(("tool-detail", 0usize)).visible());
                })
                .unwrap();
                cx.capture_screenshot(handle)
                    .unwrap()
                    .save(output.join(format!("{name}-{theme}-{width}-closed.png")))
                    .unwrap();
                cx.update_window(handle, |_, window, cx| {
                    window.click(("tool-detail", 0usize), cx);
                    assert!(window.find(("tool-raw-toggle", 0usize)).visible());
                    assert!(window.try_find(("tool-raw-input", 0usize)).is_none());
                    assert!(window.try_find(("tool-raw-result", 0usize)).is_none());
                })
                .unwrap();
                cx.capture_screenshot(handle)
                    .unwrap()
                    .save(output.join(format!("{name}-{theme}-{width}-expanded.png")))
                    .unwrap();
                if name == "extension_component" {
                    cx.update(|cx| view.read(cx).portable_preview_actual_shapes());
                }
                if theme == "dark" && width == 1000 {
                    cx.update_window(handle, |_, window, cx| {
                        window.click(("tool-raw-toggle", 0usize), cx);
                        assert!(window.find(("tool-raw-input", 0usize)).visible());
                    })
                    .unwrap();
                    cx.capture_screenshot(handle)
                        .unwrap()
                        .save(output.join(format!("{name}-{theme}-{width}-raw.png")))
                        .unwrap();
                }
            }
        }
    }
    client.shutdown().unwrap();
    println!("Native portable tool cards captured: {}", output.display());
}
