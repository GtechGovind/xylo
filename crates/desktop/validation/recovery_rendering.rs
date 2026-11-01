//! Normal chat capture with an interrupted write retained in the real Host/Store.
//! This renders offscreen and never inspects a locked desktop or uses a model.
#![allow(dead_code, unused_imports)]
extern crate gpui_kit as gpui;
#[path = "../src/assets.rs"]
mod assets;
mod capture_dir;
#[path = "../src/icons.rs"]
mod icons;
#[path = "../src/motion.rs"]
mod motion;
mod recovery_fixture;
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
        pub(crate) fn capture_chat_ready(&self) -> bool {
            !self.history_loading() && !self.messages.is_empty()
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
    let mut settings = xylo_core::AppSettings::default();
    settings.last_workspace = Some(profile.workspace.clone());
    settings.theme = "dark".into();
    profile.store.save_settings(&settings).unwrap();
    let host = recovery_fixture::host(&profile);
    let client = host.0.clone();
    let mut cx = HeadlessAppContext::with_platform(
        platform::current_platform(true).text_system(),
        Arc::new(crate::assets::AppAssets),
        platform::current_headless_renderer,
    );
    cx.allow_parking();
    cx.update(|cx| {
        gpui_kit::init(cx);
        Theme::change(ThemeMode::Dark, None, cx);
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
                |window, cx| cx.new(|cx| xylo::Xylo::new(Ok(host), window, cx)),
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
    wait(&mut cx, |cx| view.read(cx).capture_chat_ready());
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("sidebar-new-chat").visible());
        assert!(window.find_all("review-interrupted-actions").is_empty());
        assert!(window.find_all("action-review-panel").is_empty());
    })
    .unwrap();
    let output = capture_dir::fresh("recovery-rendering");
    let image = output.join("recovery.png");
    let screenshot = cx
        .capture_screenshot(handle)
        .expect("Native offscreen renderer available");
    let pixel_size = [screenshot.width(), screenshot.height()];
    screenshot.save(&image).unwrap();
    let compiled_sources = [
        ("xylo.rs", include_bytes!("../src/xylo.rs").as_slice()),
        ("chat.rs", include_bytes!("../src/xylo/chat.rs").as_slice()),
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
    .map(|(name, bytes)| (name, hex::encode(Sha256::digest(bytes))))
    .collect::<std::collections::BTreeMap<_, _>>();
    let metadata = serde_json::json!({"scope":"Actual production Host and Store; modern protocol-qualified interrupted partial file write. No model, account, external action, or physical desktop acceptance.",
        "logical_size":[1380,880],"pixel_size":pixel_size,"file":"recovery.png","sha256":hex::encode(Sha256::digest(std::fs::read(&image).unwrap())),"compiled_sources_sha256":compiled_sources});
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
        "Native uninterrupted chat layout capture: {} (actual Host/Store interrupted-write fixture, offscreen only)",
        image.canonicalize().unwrap().display()
    );
}
