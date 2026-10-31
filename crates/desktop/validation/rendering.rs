//! Native GPU/font captures of the approved workspace and Settings. All project
//! and Git content comes from a temporary repository through the real Host.
//! No model turn, user project, account credentials, or physical desktop is used.
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
            self.settings_loaded
                && self.workspace_ready
                && !self.file_browser.indexing
                && !self.file_browser.loading()
        }
        pub(crate) fn capture_file_row(&self, path: &Path) -> Option<usize> {
            self.file_browser.tree.rows.iter().position(|row| {
                row.path == path && matches!(row.content, file_browser::RowContent::Entry(_))
            })
        }
        pub(crate) fn capture_editor_ready(&self, path: &Path) -> bool {
            self.editor_path.as_deref() == Some(path)
        }
        pub(crate) fn capture_memory_ready(&self) -> bool {
            self.facts.status_message() != "Loading memories…"
        }
        pub(crate) fn capture_git_ready(&self) -> bool {
            self.git_panel.status.is_some() && self.git_panel.request.is_none()
        }
        pub(crate) fn capture_diff_ready(&self) -> bool {
            self.git_panel.diff_request.is_none()
                && self
                    .git_panel
                    .diff_lines
                    .iter()
                    .any(|line| line == "+    println!(\"Focused workspace\");")
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
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(&project)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "Git fixture: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "--quiet"]);
    git(&["add", "."]);
    git(&[
        "-c",
        "user.name=Native capture",
        "-c",
        "user.email=capture@example.invalid",
        "commit",
        "--quiet",
        "-m",
        "fixture",
    ]);
    std::fs::write(
        project.join("src/main.rs"),
        "fn main() {\n    println!(\"Focused workspace\");\n}\n",
    )
    .unwrap();
    std::fs::write(
        project.join("src/theme.rs"),
        "pub const ACCENT: u32 = 0xa7b8f0;\n",
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
    let output = capture_dir::fresh("rendering");
    let save = |cx: &mut HeadlessAppContext, name: &str| {
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            view.read(cx).capture_healthy();
        })
        .unwrap();
        cx.capture_screenshot(handle)
            .expect("Native renderer available")
            .save(output.join(name))
            .unwrap();
    };
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("xylo-brand-mark").visible());
        assert!(window.find_by_label("Send message").visible());
        assert!(window.try_find("scheduled-tasks").is_none());
    })
    .unwrap();
    save(&mut cx, "home.png");
    cx.update_window(handle, |_, window, cx| {
        window.click("toggle-work-panel", cx);
    })
    .unwrap();
    wait(&mut cx, &|view| {
        view.capture_ready() && view.capture_git_ready()
    });
    let src = project.join("src");
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        let row = view
            .read(cx)
            .capture_file_row(&src)
            .expect("Real src directory");
        window.click(("file", row), cx);
    })
    .unwrap();
    wait(&mut cx, &|view| {
        view.capture_file_row(&project.join("src/main.rs"))
            .is_some()
    });
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        let row = view
            .read(cx)
            .capture_file_row(&project.join("README.md"))
            .expect("Real README");
        window.click(("file", row), cx);
    })
    .unwrap();
    wait(&mut cx, &|view| {
        view.capture_editor_ready(&project.join("README.md"))
    });
    save(&mut cx, "files.png");
    cx.update_window(handle, |_, window, cx| {
        window.click("work-tab-changes", cx);
    })
    .unwrap();
    wait(&mut cx, &|view| view.capture_git_ready());
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        let file = window.find(("git-row", 2usize));
        assert_eq!(file.label(), Some("Open diff main.rs"));
        window.click(("git-row", 2usize), cx);
    })
    .unwrap();
    wait(&mut cx, &|view| view.capture_diff_ready());
    save(&mut cx, "changes.png");
    cx.update_window(handle, |_, window, cx| {
        window.click("settings", cx);
    })
    .unwrap();
    save(&mut cx, "settings.png");
    for (navigation, control, filename) in [
        ("nav-general", "agent-timeout-120", "settings-general.png"),
        (
            "nav-connections",
            "connector-add",
            "settings-connections.png",
        ),
        ("nav-skills", "skills-browse-plugins", "settings-skills.png"),
        ("nav-memory", "memory-new", "settings-memory.png"),
    ] {
        cx.update_window(handle, |_, window, cx| {
            window.click(navigation, cx);
            window.render_frame(cx);
            assert!(window.find(control).visible());
            assert!(window.find(navigation).visible());
            assert!(window.try_find("nav-14").is_none());
            assert!(window.try_find("nav-15").is_none());
        })
        .unwrap();
        if navigation == "nav-memory" {
            wait(&mut cx, &|view| view.capture_memory_ready());
        }
        if navigation == "nav-skills" {
            wait(&mut cx, &|view| view.validation_learning_idle());
        }
        save(&mut cx, filename);
    }
    cx.update_window(handle, |_, window, cx| {
        window.click("nav-permissions", cx);
        window.render_frame(cx);
        assert!(window.find(("permission-setting", 0usize)).visible());
    })
    .unwrap();
    save(&mut cx, "settings-permissions.png");
    cx.update_window(handle, |_, window, cx| {
        window.click("nav-general", cx);
        window.click("theme-light", cx);
    })
    .unwrap();
    save(&mut cx, "settings-light.png");
    std::fs::write(output.join("capture-scope.json"),serde_json::to_vec_pretty(&serde_json::json!({
        "scope":"Production native renderer with a real temporary Git repository and Host; no model turn, user project, connected-account simulation or physical desktop capture.",
        "size":[1380,880],"files":["home.png","files.png","changes.png","settings.png","settings-general.png","settings-connections.png","settings-permissions.png","settings-skills.png","settings-memory.png","settings-light.png"]
    })).unwrap()).unwrap();
    client.shutdown().unwrap();
    println!(
        "Native workspace and Settings captures: {}",
        output.display()
    );
}
