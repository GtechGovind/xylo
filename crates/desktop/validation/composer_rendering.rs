//! Controlled offscreen captures of the production Workbench and composer.
//! A real temporary Host/Store loads a synthetic local configuration and empty
//! project. No prompt is sent, no account is read and no device is activated.
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
mod workbench {
    include!("../src/workbench.rs");

    #[cfg(test)]
    impl Workbench {
        pub(crate) fn composer_capture_ready(&self) -> bool {
            self.settings_loaded
                && self.workspace_ready
                && self
                    .drafts
                    .validation_revision(&workbench_runtime::drafts::DraftKey {
                        session_id: self.session,
                        workspace: self.workspace.clone(),
                    })
                    .is_some()
        }

        pub(crate) fn composer_capture_theme(&self) -> &str {
            &self.settings.theme
        }

        pub(crate) fn composer_capture_prompt(
            &mut self,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) {
            self.composer.update(cx, |input, cx| {
                input.set_value("Plan the next step for this project.", window, cx);
            });
            self.draft_changed(cx);
            cx.notify();
        }

        pub(crate) fn composer_capture_healthy(&self) {
            assert!(self.error.is_none(), "Actual Host error: {:?}", self.error);
            assert!(self.host.is_some(), "A real temporary Host is required");
            assert!(
                !self.running(),
                "The fixture must never execute a model task"
            );
            assert!(
                !self.speech.busy(),
                "The fixture must not activate a microphone"
            );
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
    use sha2::{Digest, Sha256};
    use std::{
        path::PathBuf,
        sync::Arc,
        time::{Duration, Instant},
    };
    use workbench_core::{AppSettings, PermissionMode};
    use workbench_runtime::{AppPaths, host::HostClient};
    use workbench_services::Store;

    let temporary = tempfile::tempdir().unwrap();
    let workspace = temporary.path().join("Empty project");
    std::fs::create_dir(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();
    let paths = AppPaths::at(temporary.path().join("profile")).unwrap();
    let store = Store::open(&paths.database).unwrap();
    let mut settings = AppSettings {
        last_workspace: Some(workspace.clone()),
        permissions: PermissionMode::AskForWrites,
        theme: "light".into(),
        ..Default::default()
    };
    // This is a persisted display configuration, not an available/live model.
    settings.provider.model = "fixture-local-coder-model".into();
    store.save_settings(&settings).unwrap();
    let host = HostClient::spawn_at(paths).unwrap();
    let client = host.0.clone();
    let mut cx = HeadlessAppContext::with_platform(
        platform::current_platform(true).text_system(),
        Arc::new(gpui_kit::assets::Assets),
        platform::current_headless_renderer,
    );
    cx.allow_parking();
    cx.update(|cx| {
        gpui_kit::init(cx);
        Theme::change(ThemeMode::Light, None, cx);
    });
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
                |window, cx| cx.new(|cx| workbench::Workbench::new(Ok(host), window, cx)),
            )
        })
        .unwrap();

    let wait = |cx: &mut HeadlessAppContext, theme: &str| {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            cx.advance_clock(Duration::from_millis(25));
            cx.run_until_parked();
            let ready = cx.update(|cx| {
                let view = view.read(cx);
                view.composer_capture_healthy();
                view.composer_capture_ready() && view.composer_capture_theme() == theme
            });
            if ready {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "Temporary composer Host did not become ready"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    wait(&mut cx, "light");
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |view, cx| view.composer_capture_prompt(window, cx));
    })
    .unwrap();
    cx.run_until_parked();

    let output = std::env::var_os("WORKBENCH_RENDER_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../../../../work/xylo-composer-rendering")
        });
    std::fs::create_dir_all(&output).unwrap();
    let mut cases = Vec::new();
    for theme in ["light", "dark"] {
        settings.theme = theme.into();
        client.save_settings(settings.clone()).unwrap();
        wait(&mut cx, theme);
        for width in [1380_u32, 1000] {
            cx.update_window(handle, |_, window, cx| {
                window.resize(size(px(width as f32), px(880.)));
                // The controlled test platform updates native bounds directly;
                // deliver the same public callback used by real OS resizing.
                window.bounds_changed(cx);
            })
            .unwrap();
            cx.run_until_parked();
            let geometry = cx
                .update_window(handle, |_, window, cx| {
                    view.read(cx).composer_capture_healthy();
                    window.render_frame(cx);
                    assert_eq!(window.viewport_size(), size(px(width as f32), px(880.)));
                    let ids = [
                        "attach-images",
                        "paste-image-files",
                        "composer-model",
                        "permission-mode",
                        "record-voice-composer",
                        "send-prompt",
                    ];
                    let mut controls = Vec::new();
                    let mut previous_right = None;
                    let mut center_y = None::<Pixels>;
                    let mut toolbar_bottom = px(0.);
                    for id in ids {
                        let node = window.find(id);
                        assert!(node.visible(), "{theme}/{width}: {id} is clipped or hidden");
                        let bounds = node.bounds();
                        assert!(bounds.size.width > px(0.) && bounds.size.height > px(0.));
                        assert!(
                            bounds.left() >= px(0.) && bounds.right() <= px(width as f32)
                                && bounds.top() >= px(0.) && bounds.bottom() <= px(880.),
                            "{theme}/{width}: {id} leaves the actual viewport"
                        );
                        let center = bounds.top() + bounds.size.height / 2.;
                        if let Some(expected) = center_y {
                            assert!(
                                (center - expected).abs() <= px(1.),
                                "{theme}/{width}: {id} wraps outside the toolbar row"
                            );
                        } else {
                            center_y = Some(center);
                        }
                        if let Some(right) = previous_right {
                            assert!(bounds.left() >= right, "{id} overlaps a previous control");
                        }
                        previous_right = Some(bounds.right());
                        toolbar_bottom = toolbar_bottom.max(bounds.bottom());
                        controls.push(serde_json::json!({
                            "id":id,"label":node.label(),
                            "bounds":[f32::from(bounds.left()),f32::from(bounds.top()),
                                f32::from(bounds.size.width),f32::from(bounds.size.height)],
                        }));
                    }
                    let message = window.find_by_label("Message");
                    assert!(message.visible(), "The real composer input must remain visible");
                    let bounds = message.bounds();
                    let envelope_height = toolbar_bottom - bounds.top();
                    assert!(bounds.bottom() <= toolbar_bottom);
                    assert!(
                        envelope_height > px(32.) && envelope_height <= px(160.),
                        "{theme}/{width}: one-line input and toolbar are not compact: {envelope_height:?}"
                    );
                    serde_json::json!({
                        "theme":theme,"logical_viewport":[width,880],"controls":controls,
                        "input_bounds":[f32::from(bounds.left()),f32::from(bounds.top()),
                            f32::from(bounds.size.width),f32::from(bounds.size.height)],
                        "input_to_toolbar_height":f32::from(envelope_height),
                        "same_row_within_viewport":true,
                    })
                })
                .unwrap();
            let screenshot = cx
                .capture_screenshot(handle)
                .expect("Native renderer is required");
            let filename = format!("composer-{theme}-{width}.png");
            let destination = output.join(&filename);
            assert!(
                !destination.exists(),
                "Capture evidence must not be overwritten"
            );
            screenshot.save(&destination).unwrap();
            cases.push(serde_json::json!({
                "geometry":geometry,"image":filename,
                "pixels":[screenshot.width(),screenshot.height()],
                "sha256":format!("{:x}",Sha256::digest(std::fs::read(destination).unwrap())),
            }));
        }
    }
    let compiled_sources = [
        (
            "workbench.rs",
            include_bytes!("../src/workbench.rs").as_slice(),
        ),
        (
            "chat.rs",
            include_bytes!("../src/workbench/chat.rs").as_slice(),
        ),
        (
            "attachments.rs",
            include_bytes!("../src/workbench/attachments.rs").as_slice(),
        ),
        (
            "speech.rs",
            include_bytes!("../src/workbench/speech.rs").as_slice(),
        ),
        ("ui.rs", include_bytes!("../src/ui.rs").as_slice()),
        (
            "composer_rendering.rs",
            include_bytes!("composer_rendering.rs").as_slice(),
        ),
        ("native-Cargo.lock", include_bytes!("Cargo.lock").as_slice()),
    ]
    .into_iter()
    .map(|(path, bytes)| (path, format!("{:x}", Sha256::digest(bytes))))
    .collect::<std::collections::BTreeMap<_, _>>();
    let metadata = serde_json::json!({
        "schema":1,
        "scope":"Controlled offscreen production Workbench capture with real temporary Host/Store, native platform fonts and GPU. Synthetic persisted model label, no inference/accounts/microphone or physical desktop acceptance.",
        "cases":cases,"compiled_sources":compiled_sources,
        "source_errors_suppressed":false,
        "controls_clicked":false,
    });
    let metadata_path = output.join("composer-rendering.json");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(metadata_path)
        .unwrap();
    std::io::Write::write_all(
        &mut file,
        serde_json::to_string_pretty(&metadata).unwrap().as_bytes(),
    )
    .unwrap();
    client.shutdown().unwrap();
    cx.run_until_parked();
    println!(
        "Controlled native composer rendering: {}\n{metadata}",
        output.display()
    );
}
