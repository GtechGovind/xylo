//! Actual Host/Store/helper missing-original document recovery capture.
//! This renders offscreen and never inspects a locked desktop or uses a model.
#![allow(dead_code, unused_imports)]
extern crate gpui_kit as gpui;
mod document_recovery_fixture;
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
        pub(crate) fn capture_services_ready(&self) -> bool {
            self.settings_loaded && self.workspace_ready
        }
        pub(crate) fn capture_open(
            &mut self,
            path: PathBuf,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) {
            self.document_resume_saved(path, window, cx);
        }
        pub(crate) fn capture_missing_ready(&self) -> bool {
            self.documents_validation_missing()
                && self.documents_validation_saved()
                && self.documents_validation_no_reads()
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
    let profile = document_recovery_fixture::seeded(xylo_core::DocumentKind::Spreadsheet);
    let delivery = Arc::new(document_recovery_fixture::Delivery::default());
    let host = document_recovery_fixture::host(&profile, delivery);
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
    wait(&mut cx, |cx| view.read(cx).capture_services_ready());
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |view, cx| {
            view.capture_open(profile.original.clone(), window, cx)
        })
    })
    .unwrap();
    wait(&mut cx, |cx| view.read(cx).capture_missing_ready());
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click(("document-recovered-change", 1usize), cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("new-chat").visible());
        assert!(window.find("document-find-original").visible());
        assert!(window.find(("document-recovered-change", 0usize)).visible());
        assert!(window.find(("document-recovered-change", 1usize)).visible());
        assert!(window.find("document-copy-recovered").visible());
        assert!(
            window.find("document-find-original").bounds().origin.x
                > window.find("new-chat").bounds().right()
        );
    })
    .unwrap();
    let output = std::env::var_os("XYLO_RENDER_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../../docs/native-preview")
        });
    std::fs::create_dir_all(&output).unwrap();
    let image = output.join("document-recovery.png");
    let screenshot = cx
        .capture_screenshot(handle)
        .expect("Native offscreen renderer available");
    let pixel_size = [screenshot.width(), screenshot.height()];
    screenshot.save(&image).unwrap();
    let compiled_sources = [
        ("xylo.rs", include_bytes!("../src/xylo.rs").as_slice()),
        (
            "documents.rs",
            include_bytes!("../src/xylo/documents.rs").as_slice(),
        ),
        (
            "documents_drafts.rs",
            include_bytes!("../src/xylo/documents_drafts.rs").as_slice(),
        ),
        (
            "documents_missing.rs",
            include_bytes!("../src/xylo/documents_missing.rs").as_slice(),
        ),
        (
            "documents_view.rs",
            include_bytes!("../src/xylo/documents_view.rs").as_slice(),
        ),
        (
            "document_drafts.rs",
            include_bytes!("../../services/src/document_drafts.rs").as_slice(),
        ),
        (
            "relink.rs",
            include_bytes!("../../services/src/document_drafts/relink.rs").as_slice(),
        ),
        (
            "source_file.rs",
            include_bytes!("../../services/src/document_drafts/relink/source_file.rs").as_slice(),
        ),
        (
            "document_control.rs",
            include_bytes!("../../runtime/src/host/document_control.rs").as_slice(),
        ),
        (
            "document_recovery_fixture.rs",
            include_bytes!("document_recovery_fixture.rs").as_slice(),
        ),
        (
            "document_recovery_rendering.rs",
            include_bytes!("document_recovery_rendering.rs").as_slice(),
        ),
        ("Cargo.lock", include_bytes!("Cargo.lock").as_slice()),
    ]
    .into_iter()
    .map(|(name, bytes)| (name, format!("{:x}", Sha256::digest(bytes))))
    .collect::<std::collections::BTreeMap<_, _>>();
    let metadata = serde_json::json!({"scope":"Actual production Host, Store and native helper; missing-original XLSX draft with typed text/formula changes. No Office file reconstructed, no model/account or physical desktop acceptance.",
        "logical_size":[1380,880],"pixel_size":pixel_size,"file":"document-recovery.png","sha256":format!("{:x}",Sha256::digest(std::fs::read(&image).unwrap())),"compiled_sources_sha256":compiled_sources});
    std::fs::write(
        output.join("document-recovery.json"),
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
        "Native missing-original document layout capture: {} (actual Host/Store/helper missing-original fixture, offscreen only)",
        image.canonicalize().unwrap().display()
    );
}
