//! Controlled native GPU/font capture with a real temporary project and Host.
//! No account, model task, user project, or physical desktop is accessed.
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
        pub(crate) fn tree_capture_ready(&self) -> bool {
            self.settings_loaded
                && self.workspace_ready
                && !self.file_browser.indexing
                && !self.file_browser.loading()
        }
        pub(crate) fn tree_capture_page(&mut self, cx: &mut Context<Self>) {
            self.page = Page::Files;
            cx.notify();
        }
        pub(crate) fn tree_capture_index(&self, path: &Path) -> usize {
            self.file_browser
                .tree
                .rows
                .iter()
                .position(|row| {
                    row.path == path && matches!(row.content, file_browser::RowContent::Entry(_))
                })
                .expect("visible fixture path")
        }
        pub(crate) fn tree_capture_contains(&self, path: &Path) -> bool {
            self.file_browser
                .entries
                .iter()
                .any(|entry| entry.path == path)
        }
        pub(crate) fn tree_capture_editor_ready(&self, path: &Path) -> bool {
            self.editor_path.as_deref() == Some(path)
        }
        pub(crate) fn tree_capture_healthy(&self) {
            assert!(self.error.is_none(), "Actual Host error: {:?}", self.error);
            assert!(!self.running());
        }
    }
}
fn main() {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    run();
}
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn run() {
    use gpui_kit::{
        component::{Theme, ThemeMode},
        prelude::*,
        test::{TestQueryExt, TestWindowExt},
        *,
    };
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("Xylo sample");
    std::fs::create_dir_all(project.join("src/ui")).unwrap();
    std::fs::create_dir_all(project.join("docs")).unwrap();
    std::fs::write(
        project.join("README.md"),
        "# Xylo sample\n\nA temporary native file tree fixture.\n",
    )
    .unwrap();
    std::fs::write(
        project.join("Cargo.toml"),
        "[package]\nname = \"xylo-sample\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(
        project.join("src/main.rs"),
        "fn main() {\n    println!(\"Hello, Xylo!\");\n}\n",
    )
    .unwrap();
    std::fs::write(
        project.join("src/ui/mod.rs"),
        "//! Sample user interface module.\n",
    )
    .unwrap();
    std::fs::write(
        project.join("docs/getting-started.md"),
        "# Getting started\n",
    )
    .unwrap();
    let project = project.canonicalize().unwrap();
    let paths = xylo_runtime::AppPaths::at(temporary.path().join("profile")).unwrap();
    let store = xylo_services::Store::open(&paths.database).unwrap();
    store
        .save_settings(&xylo_core::AppSettings {
            theme: "dark".into(),
            last_workspace: Some(project.clone()),
            ..Default::default()
        })
        .unwrap();
    let host = xylo_runtime::host::HostClient::spawn_at(paths).unwrap();
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
                        size(px(1280.), px(820.)),
                    ))),
                    show: false,
                    ..Default::default()
                },
                cx,
                |window, cx| cx.new(|cx| xylo::Xylo::new(Ok(host), window, cx)),
            )
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let wait = |cx: &mut HeadlessAppContext, target: Option<&std::path::Path>| {
        loop {
            cx.run_until_parked();
            let ready = cx.update(|cx| {
                let state = view.read(cx);
                state.tree_capture_ready()
                    && target.is_none_or(|path| state.tree_capture_contains(path))
            });
            if ready {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Temporary Host tree did not load"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    wait(&mut cx, Some(&project.join("src")));
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |state, cx| state.tree_capture_page(cx));
        window.render_frame(cx);
    })
    .unwrap();
    for (directory, child) in [
        (project.join("src"), project.join("src/main.rs")),
        (project.join("src/ui"), project.join("src/ui/mod.rs")),
    ] {
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            let index = view.read(cx).tree_capture_index(&directory);
            window.click(("file", index), cx);
            window.render_frame(cx);
        })
        .unwrap();
        wait(&mut cx, Some(&child));
    }
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        let index = view
            .read(cx)
            .tree_capture_index(&project.join("src/main.rs"));
        window.click(("file", index), cx);
        window.render_frame(cx);
    })
    .unwrap();
    loop {
        cx.run_until_parked();
        if cx.update(|cx| {
            view.read(cx)
                .tree_capture_editor_ready(&project.join("src/main.rs"))
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Temporary fixture editor did not open"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let proof = cx.update_window(handle, |_,window,cx| {
        window.render_frame(cx); let state=view.read(cx); state.tree_capture_healthy();
        let mut rows=Vec::new();
        for relative in ["src", "src/ui", "src/ui/mod.rs", "src/main.rs"] {
            let index=state.tree_capture_index(&project.join(relative)); let bounds=window.find(("file",index)).bounds();
            rows.push(serde_json::json!({"path":relative,"x":f32::from(bounds.origin.x),"y":f32::from(bounds.origin.y),"width":f32::from(bounds.size.width)}));
        }
        serde_json::json!({"scope":"Controlled native GPU/font fixture using real temporary Host/project, no inference or user files", "rows":rows})
    }).unwrap();
    let output = std::env::var_os("XYLO_RENDER_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| "target/native-file-tree-preview".into());
    std::fs::create_dir_all(&output).unwrap();
    cx.capture_screenshot(handle)
        .unwrap()
        .save(output.join("file-tree-fixture.png"))
        .unwrap();
    std::fs::write(
        output.join("geometry.json"),
        serde_json::to_vec_pretty(&proof).unwrap(),
    )
    .unwrap();
    println!("Native file tree fixture: {}\n{proof}", output.display());
}
