//! Actual Host/Store interrupted-write capture through the production controls.
//! This renders offscreen and never inspects a locked desktop or uses a model.
#![allow(dead_code, unused_imports)]
extern crate gpui_kit as gpui;
mod recovery_fixture;
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
    impl Workbench {
        // Harness-only observations and a production navigation call. No
        // recovery catalogue, permission, receipt or outcome is injected.
        pub(crate) fn capture_services_ready(&self, session: Uuid) -> bool {
            self.settings_loaded
                && self.workspace_ready
                && self
                    .conversations
                    .iter()
                    .any(|conversation| conversation.id == session)
        }
        pub(crate) fn capture_open(
            &mut self,
            session: Uuid,
            workspace: PathBuf,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) {
            self.open_conversation(session, workspace, window, cx);
        }
        pub(crate) fn capture_review_ready(&self, selected: bool) -> bool {
            self.validation_review_ready()
                && self.validation_review_items().len() == 1
                && (!selected || self.validation_review_detail().is_some())
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
        sync::Arc,
        time::{Duration, Instant},
    };
    let profile = recovery_fixture::seeded(1);
    let delivery = Arc::new(recovery_fixture::Delivery::default());
    let host = recovery_fixture::host(&profile, delivery);
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
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: Default::default(),
                        size: size(px(1380.), px(880.)),
                    })),
                    focus: false,
                    show: false,
                    ..Default::default()
                },
                cx,
                |window, cx| cx.new(|cx| workbench::Workbench::new(Ok(host), window, cx)),
            )
        })
        .unwrap();
    fn wait(cx: &mut HeadlessAppContext, condition: impl Fn(&mut App) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(25);
        loop {
            cx.advance_clock(Duration::from_millis(25));
            cx.run_until_parked();
            if cx.update(|cx| condition(cx)) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "Actual recovery capture did not finish loading"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    wait(&mut cx, |cx| {
        view.read(cx).capture_services_ready(profile.session)
    });
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |view, cx| {
            view.capture_open(profile.session, profile.workspace.clone(), window, cx)
        })
    })
    .unwrap();
    wait(&mut cx, |cx| view.read(cx).capture_review_ready(false));
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("review-interrupted-actions", cx);
        window.render_frame(cx);
        window.click(("review-select-action", 0usize), cx);
    })
    .unwrap();
    wait(&mut cx, |cx| view.read(cx).capture_review_ready(true));
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("new-chat").visible());
        assert!(window.find(("review-select-action", 0usize)).visible());
        assert!(window.find("review-open-target").visible());
        assert!(window.find(("review-artifact", 0usize)).visible());
        assert_eq!(window.find("review-record-outcome").disabled(), Some(true));
        assert!(window.find("review-record-outcome").visible());
        let action = window.find(("review-select-action", 0usize));
        let target = window.find("review-open-target");
        assert!(target.bounds().origin.x > action.bounds().right());
    })
    .unwrap();
    let output = std::env::var_os("WORKBENCH_RENDER_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../../docs/native-preview")
        });
    std::fs::create_dir_all(&output).unwrap();
    let image = output.join("recovery.png");
    let screenshot = cx
        .capture_screenshot(handle)
        .expect("Native offscreen renderer available");
    let pixel_size = [screenshot.width(), screenshot.height()];
    screenshot.save(&image).unwrap();
    let compiled_sources = [
        (
            "workbench.rs",
            include_bytes!("../src/workbench.rs").as_slice(),
        ),
        (
            "recovery.rs",
            include_bytes!("../src/workbench/recovery.rs").as_slice(),
        ),
        (
            "recovery_forms.rs",
            include_bytes!("../src/workbench/recovery_forms.rs").as_slice(),
        ),
        (
            "recovery_view.rs",
            include_bytes!("../src/workbench/recovery_view.rs").as_slice(),
        ),
        (
            "recovery_fixture.rs",
            include_bytes!("recovery_fixture.rs").as_slice(),
        ),
        (
            "recovery_rendering.rs",
            include_bytes!("recovery_rendering.rs").as_slice(),
        ),
        ("Cargo.lock", include_bytes!("Cargo.lock").as_slice()),
    ]
    .into_iter()
    .map(|(name, bytes)| (name, format!("{:x}", Sha256::digest(bytes))))
    .collect::<std::collections::BTreeMap<_, _>>();
    let metadata = serde_json::json!({"scope":"Actual production Host and Store; modern protocol-qualified interrupted partial file write. No model, account, external action, or physical desktop acceptance.",
        "logical_size":[1380,880],"pixel_size":pixel_size,"file":"recovery.png","sha256":format!("{:x}",Sha256::digest(std::fs::read(&image).unwrap())),"compiled_sources_sha256":compiled_sources});
    std::fs::write(
        output.join("recovery.json"),
        serde_json::to_vec_pretty(&metadata).unwrap(),
    )
    .unwrap();
    client.shutdown().unwrap();
    wait(&mut cx, |_| {
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(profile.paths.data.join("host.lock"))
            .unwrap();
        lock.try_lock().is_ok()
    });
    println!(
        "Native recovery layout capture: {} (actual Host/Store interrupted-write fixture, offscreen only)",
        image.canonicalize().unwrap().display()
    );
}
