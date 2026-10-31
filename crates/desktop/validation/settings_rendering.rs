//! Native GPU/font captures of every Settings destination in both themes at
//! 1000 and 1400 px wide, through a real temporary Host. No model turn, user
//! project, account credentials or physical desktop is used. Pages that list
//! remote catalogs may contact their public sources.
#![allow(dead_code, unused_imports)]
extern crate gpui_kit as gpui;
#[path = "../src/assets.rs"]
mod assets;
mod capture_dir;
#[path = "../src/icons.rs"]
mod icons;
#[path = "../src/motion.rs"]
mod motion;
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
        pub(crate) fn capture_ready(&self) -> bool {
            self.settings_loaded && self.workspace_ready
        }
        pub(crate) fn capture_healthy(&self) {
            assert!(!self.running(), "Capture must not invoke a model turn");
        }
    }
}

/// Settings destinations in sidebar order: navigation id and file stem.
const PAGES: [(&str, &str); 12] = [
    ("nav-general", "general"),
    ("nav-permissions", "permissions"),
    ("nav-notifications", "notifications"),
    ("nav-providers", "providers"),
    ("nav-images", "images"),
    ("nav-voice", "voice"),
    ("nav-connections", "connections"),
    ("nav-plugins", "plugins"),
    ("nav-environments", "environments"),
    ("nav-skills", "skills"),
    ("nav-memory", "memory"),
    ("nav-downloads", "downloads"),
];

fn main() {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    for width in [1000., 1400.] {
        render(width);
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn render(width: f32) {
    use gpui_kit::{
        prelude::*,
        test::{TestQueryExt, TestWindowExt},
        *,
    };
    use std::{
        path::PathBuf,
        sync::Arc,
        time::{Duration, Instant},
    };
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("northstar");
    std::fs::create_dir_all(project.join("src")).unwrap();
    std::fs::write(project.join("README.md"), "# Northstar\n").unwrap();
    let project = project.canonicalize().unwrap();
    let paths = xylo_runtime::AppPaths::at(temporary.path().join("profile")).unwrap();
    xylo_services::Store::open(&paths.database)
        .unwrap()
        .save_settings(&xylo_core::AppSettings {
            theme: "light".into(),
            last_workspace: Some(project.clone()),
            ..Default::default()
        })
        .unwrap();
    let host = xylo_runtime::host::HostClient::spawn_at(paths).unwrap();
    let client = host.0.clone();
    let mut cx = HeadlessAppContext::with_platform(
        platform::current_platform(true).text_system(),
        Arc::new(crate::assets::AppAssets),
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
                        size(px(width), px(860.)),
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
    // Lets host answers arrive and entrance motion finish before a capture.
    let settle = |cx: &mut HeadlessAppContext, duration: Duration| {
        let deadline = Instant::now() + duration;
        while Instant::now() < deadline {
            cx.run_until_parked();
            cx.advance_clock(Duration::from_millis(5));
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    let ready = Instant::now() + Duration::from_secs(15);
    while !cx.update(|cx| view.read(cx).capture_ready()) {
        assert!(Instant::now() < ready, "Real Host did not become ready");
        settle(&mut cx, Duration::from_millis(50));
    }
    let output = capture_dir::fresh("settings-rendering");
    let mut files = Vec::new();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        window.click("settings", cx);
    })
    .unwrap();
    for (theme, toggle) in [("light", "theme-light"), ("dark", "theme-dark")] {
        cx.update_window(handle, |_, window, cx| {
            window.click("nav-general", cx);
            window.render_frame(cx);
            window.click(toggle, cx);
        })
        .unwrap();
        for (navigation, stem) in PAGES {
            cx.update_window(handle, |_, window, cx| {
                window.click(navigation, cx);
                window.render_frame(cx);
                assert!(window.find(navigation).visible());
            })
            .unwrap();
            settle(&mut cx, Duration::from_millis(700));
            cx.update_window(handle, |_, window, cx| {
                window.render_frame(cx);
                view.read(cx).capture_healthy();
            })
            .unwrap();
            let name = format!("{stem}-{theme}-{}.png", width as u32);
            cx.capture_screenshot(handle)
                .expect("Native renderer available")
                .save(output.join(&name))
                .unwrap();
            files.push(name);
        }
    }
    std::fs::write(
        output.join(format!("capture-scope-{}.json", width as u32)),
        serde_json::to_vec_pretty(&serde_json::json!({
            "scope": "Production native renderer with a temporary project and Host.",
            "size": [width, 860],
            "files": files,
        }))
        .unwrap(),
    )
    .unwrap();
    client.shutdown().unwrap();
    println!("Settings captures: {}", output.display());
}
