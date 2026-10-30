//! Native portable extension lifecycle through a real temporary Host and catalog.
//! Captures use production GPUI controls, native fonts/GPU and a real local skill.
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
        pub(crate) fn extension_review_request(
            &mut self,
            name: &str,
            args: serde_json::Value,
            cx: &mut Context<Self>,
        ) {
            self.extension_request(name, args, cx);
        }
        pub(crate) fn extension_review_theme(&self) -> &str {
            &self.settings.theme
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
    let package = temporary.path().join("workspace-diagnostics");
    std::fs::create_dir_all(package.join("skills/check-project")).unwrap();
    std::fs::write(package.join("skills/check-project/SKILL.md"), "---\nname: check-project\ndescription: Inspect the selected project and report actionable diagnostics.\n---\nRead project configuration before running checks. Report evidence and preserve user changes.\n").unwrap();
    std::fs::write(package.join(".mcp.json"), serde_json::json!({"mcpServers":{"project_diagnostics":{"command":"node","args":["${CLAUDE_PLUGIN_ROOT}/mcp.cjs"]}}}).to_string()).unwrap();
    std::fs::write(package.join("mcp.cjs"), r#"const lines=require('node:readline').createInterface({input:process.stdin});lines.on('line',line=>{const m=JSON.parse(line);if(!('id'in m))return;let result={};if(m.method==='initialize')result={protocolVersion:m.params.protocolVersion,capabilities:{tools:{}},serverInfo:{name:'project-diagnostics',version:'1'}};else if(m.method==='tools/list')result={tools:[{name:'project_summary',description:'Read the supplied project summary',inputSchema:{type:'object'},annotations:{readOnlyHint:true}}]};else if(m.method==='tools/call')result={content:[{type:'text',text:'Diagnostic fixture completed'}],isError:false};console.log(JSON.stringify({jsonrpc:'2.0',id:m.id,result}));});"#).unwrap();
    std::fs::create_dir_all(package.join("hooks")).unwrap();
    std::fs::write(package.join("hooks/hooks.json"),serde_json::json!({"hooks":{"PostToolUse":[{"hooks":[{"type":"command","command":"printf verified > native-hook-verified.txt"}]}]}}).to_string()).unwrap();
    std::fs::write(package.join(".lsp.json"),serde_json::json!({"project_notes":{"command":"node","args":["${CLAUDE_PLUGIN_ROOT}/lsp.cjs"],"extensionToLanguage":{".txt":"plaintext"}}}).to_string()).unwrap();
    std::fs::write(package.join("lsp.cjs"),r#"let buffer=Buffer.alloc(0);process.stdin.on('data',data=>{buffer=Buffer.concat([buffer,data]);for(;;){const end=buffer.indexOf('\r\n\r\n');if(end<0)return;const len=Number(buffer.subarray(0,end).toString().match(/Content-Length: (\d+)/i)[1]);if(buffer.length<end+4+len)return;const m=JSON.parse(buffer.subarray(end+4,end+4+len));buffer=buffer.subarray(end+4+len);if(m.method==='exit'){process.exit(0);}if(!('id'in m))continue;let result=m.method==='initialize'?{capabilities:{hoverProvider:true},serverInfo:{name:'project-notes',version:'1'}}:m.method==='shutdown'?null:{contents:'Local project notes'};const response=Buffer.from(JSON.stringify({jsonrpc:'2.0',id:m.id,result}));process.stdout.write(`Content-Length: ${response.length}\r\n\r\n`);process.stdout.write(response);}});"#).unwrap();
    let catalog = xylo_services::extensions::ExtensionService::new(
        temporary.path().join("profile/extensions"),
    );
    let wait_controls = |cx: &mut HeadlessAppContext, id: &'static str| {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            cx.run_until_parked();
            let ready = cx
                .update_window(handle, |_, window, cx| {
                    window.render_frame(cx);
                    window
                        .try_find(id)
                        .is_some_and(|node| node.disabled() != Some(true))
                })
                .unwrap();
            if ready {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Extension UI request did not settle: {id}"
            );
            cx.advance_clock(Duration::from_millis(2));
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    cx.update_window(handle, |_, window, cx| {
        window.click("settings", cx);
        window.click("nav-skills", cx);
        window.press("tab", cx);
        assert_eq!(
            window.find("nav-plugins").focused(),
            Some(true),
            "Tab from Skills must reach Plugins"
        );
    })
    .unwrap();
    cx.capture_screenshot(handle)
        .unwrap()
        .save(output.join("plugins-keyboard-focus.png"))
        .unwrap();
    cx.update_window(handle, |_, window, cx| {
        window.press("enter", cx);
        window.render_frame(cx);
        assert!(
            window.find("extensions-refresh").visible(),
            "Keyboard Enter must activate Plugins"
        );
        window.click("nav-skills", cx);
        window.render_frame(cx);
        assert!(window.find("manage-portable-skills").visible());
    })
    .unwrap();
    wait_controls(&mut cx, "refresh-learning");
    cx.capture_screenshot(handle)
        .unwrap()
        .save(output.join("saved-procedures-link.png"))
        .unwrap();
    cx.update_window(handle, |_, window, cx| {
        window.click("manage-portable-skills", cx);
        window.render_frame(cx);
        assert!(window.find("extensions-refresh").visible());
        window.click("nav-plugins", cx);
    })
    .unwrap();
    wait_controls(&mut cx, "extensions-refresh");
    cx.update_window(handle, |_, _, cx| {
        view.update(cx, |v, cx| {
            v.extension_review_request(
                "extension_install_local",
                serde_json::json!({"path":package}),
                cx,
            )
        })
    })
    .unwrap();
    wait_controls(&mut cx, "extensions-refresh");
    let installed = catalog.list().unwrap();
    assert_eq!(
        installed.len(),
        1,
        "Actual Host must install a real local skill package"
    );
    assert_eq!(
        installed[0].name, "workspace-diagnostics",
        "Manifest-free packages must preserve the selected folder identity"
    );
    assert!(!installed[0].enabled);
    assert_eq!(installed[0].components[0].name, "check-project");
    assert!(!installed[0].components[0].description.is_empty());
    let mcp = installed[0]
        .components
        .iter()
        .find(|c| c.name == "mcpServers")
        .unwrap();
    assert_eq!(
        mcp.readiness,
        xylo_services::extensions::Readiness::NeedsSetup
    );
    assert!(mcp.evidence.is_none());
    let package_id = installed[0].id.clone();
    cx.update_window(handle, |_, window, cx| {
        window.click(("extension-enable", 0usize), cx);
    })
    .unwrap();
    wait_controls(&mut cx, "extensions-refresh");
    assert!(
        catalog.list().unwrap()[0].enabled,
        "Actual Host must persist enable"
    );
    let mcp_index = installed[0]
        .components
        .iter()
        .position(|c| c.name == "mcpServers")
        .unwrap();
    cx.update_window(handle, |_, window, cx| {
        for _ in 0..30 {
            if window.find(("extension-configure", mcp_index)).focused() == Some(true) {
                break;
            }
            window.press("tab", cx);
        }
        assert_eq!(
            window.find(("extension-configure", mcp_index)).focused(),
            Some(true),
            "Configure must be reachable by native Tab"
        );
    })
    .unwrap();
    cx.capture_screenshot(handle)
        .unwrap()
        .save(output.join("configure-keyboard-focus.png"))
        .unwrap();
    cx.update_window(handle, |_, window, cx| window.press("enter", cx))
        .unwrap();
    wait_controls(&mut cx, "extension-setup-save");
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(window.find("extension-setup-verify").disabled(), Some(true));
    })
    .unwrap();
    cx.capture_screenshot(handle)
        .unwrap()
        .save(output.join("mcp-setup-initial.png"))
        .unwrap();
    cx.update_window(handle, |_, window, cx| {
        window.click("extension-setup-save", cx)
    })
    .unwrap();
    wait_controls(&mut cx, "extension-setup-verify");
    let configured = catalog.component_status(&package_id, "mcpServers").unwrap();
    assert_eq!(configured.setup.workspace, project);
    assert_ne!(
        configured.readiness,
        xylo_services::extensions::Readiness::ExecutionVerified,
        "Saving setup must not claim execution"
    );
    cx.update_window(handle, |_, window, cx| {
        window.click("extension-setup-verify", cx)
    })
    .unwrap();
    wait_controls(&mut cx, "extension-setup-verify");
    let verified = catalog.component_status(&package_id, "mcpServers").unwrap();
    assert_eq!(
        verified.readiness,
        xylo_services::extensions::Readiness::ExecutionVerified,
        "Actual MCP verification must execute: {verified:?}"
    );
    cx.capture_screenshot(handle)
        .unwrap()
        .save(output.join("mcp-setup-verified.png"))
        .unwrap();
    cx.update_window(handle, |_, window, cx| {
        window.click("extension-setup-advanced", cx);
        let field = window.find_by_label("Non-secret environment variables");
        window.click(field.path().last().unwrap().clone(), cx);
        window.input("XYLO_DIAGNOSTIC_MODE=review", cx);
        window.render_frame(cx);
        assert_eq!(
            window.find("extension-setup-verify").disabled(),
            Some(true),
            "Dirty setup must block verification"
        );
        assert_ne!(window.find("extension-setup-save").disabled(), Some(true));
    })
    .unwrap();
    cx.capture_screenshot(handle)
        .unwrap()
        .save(output.join("mcp-setup-dirty.png"))
        .unwrap();
    cx.update_window(handle, |_, window, cx| {
        window.click("extension-setup-back", cx)
    })
    .unwrap();
    for theme in ["dark", "light"] {
        let mut settings = xylo_core::AppSettings::default();
        settings.theme = theme.into();
        settings.last_workspace = Some(project.clone());
        client.save_settings(settings).unwrap();
        wait(&mut cx, &|v| v.extension_review_theme() == theme);
        for width in [1000_u32, 1380, 1920] {
            cx.update_window(handle, |_, window, cx| {
                window.resize(size(px(width as f32), px(880.)));
                window.bounds_changed(cx);
            })
            .unwrap();
            cx.run_until_parked();
            for (tab, label) in [(1usize, "installed"), (0, "discover"), (2, "sources")] {
                cx.update_window(handle, |_, window, cx| {
                    window.click(("extensions-tab", tab), cx);
                    window.render_frame(cx);
                    assert!(window.find("nav-plugins").visible());
                    for i in 0usize..3 {
                        let tab = window.find(("extensions-tab", i));
                        assert!(tab.visible());
                        let b = tab.bounds();
                        assert!(b.right() <= px(width as f32) && b.left() >= px(0.));
                    }
                    assert!(window.find("extensions-local").visible());
                    if label == "installed" {
                        assert_eq!(
                            window.find(("extension-enable", 0usize)).label().as_deref(),
                            Some("Disable")
                        );
                    }
                })
                .unwrap();
                cx.capture_screenshot(handle)
                    .unwrap()
                    .save(output.join(format!("plugins-{theme}-{width}-{label}.png")))
                    .unwrap();
                if label == "installed" {
                    cx.update_window(handle, |_, window, cx| {
                        window.click(("extension-configure", mcp_index), cx)
                    })
                    .unwrap();
                    wait_controls(&mut cx, "extension-setup-verify");
                    cx.update_window(handle, |_, window, cx| {
                        window.click("extension-setup-advanced", cx)
                    })
                    .unwrap();
                    cx.capture_screenshot(handle)
                        .unwrap()
                        .save(output.join(format!("mcp-setup-{theme}-{width}-top.png")))
                        .unwrap();
                    cx.update_window(handle, |_, window, cx| {
                        window.scroll(
                            "extension-setup-advanced",
                            ScrollDelta::Pixels(point(px(0.), px(-600.))),
                            cx,
                        );
                        for id in ["extension-setup-save", "extension-setup-verify"] {
                            let node = window.find(id);
                            assert!(
                                node.visible(),
                                "Setup action must be reachable by native scrolling: {id}"
                            );
                            let b = node.bounds();
                            assert!(b.left() >= px(0.) && b.right() <= px(width as f32));
                        }
                    })
                    .unwrap();
                    cx.capture_screenshot(handle)
                        .unwrap()
                        .save(output.join(format!("mcp-setup-{theme}-{width}-bottom.png")))
                        .unwrap();
                    cx.update_window(handle, |_, window, cx| {
                        window.scroll(
                            "extension-setup-verify",
                            ScrollDelta::Pixels(point(px(0.), px(600.))),
                            cx,
                        );
                        window.click("extension-setup-back", cx);
                    })
                    .unwrap();
                }
            }
        }
    }
    cx.update_window(handle, |_, window, cx| {
        window.click(("extensions-tab", 1usize), cx)
    })
    .unwrap();
    for component in ["hooks", "lspServers"] {
        let component_index = installed[0]
            .components
            .iter()
            .position(|c| c.name == component)
            .unwrap();
        cx.update_window(handle, |_, window, cx| {
            window.click(("extension-configure", component_index), cx)
        })
        .unwrap();
        wait_controls(&mut cx, "extension-setup-save");
        if component == "hooks" {
            assert!(!project.join("native-hook-verified.txt").exists());
            cx.update_window(handle, |_, window, cx| {
                window.click(("extension-hook-event", 1usize), cx)
            })
            .unwrap();
        }
        cx.update_window(handle, |_, window, cx| {
            window.click("extension-setup-save", cx)
        })
        .unwrap();
        wait_controls(&mut cx, "extension-setup-verify");
        let configured = catalog.component_status(&package_id, component).unwrap();
        assert_ne!(
            configured.readiness,
            xylo_services::extensions::Readiness::ExecutionVerified
        );
        if component == "hooks" {
            assert_eq!(
                configured.setup.enabled_events,
                vec!["PostToolUse".to_owned()]
            );
            assert!(
                !project.join("native-hook-verified.txt").exists(),
                "Save must not run a hook"
            );
        }
        cx.update_window(handle, |_, window, cx| {
            window.click("extension-setup-verify", cx)
        })
        .unwrap();
        wait_controls(&mut cx, "extension-setup-verify");
        let verified = catalog.component_status(&package_id, component).unwrap();
        assert_eq!(
            verified.readiness,
            xylo_services::extensions::Readiness::ExecutionVerified,
            "Actual {component} protocol verification must execute: {verified:?}"
        );
        if component == "hooks" {
            assert_eq!(
                std::fs::read_to_string(project.join("native-hook-verified.txt")).unwrap(),
                "verified"
            );
        }
        cx.capture_screenshot(handle)
            .unwrap()
            .save(output.join(format!("{component}-setup-verified.png")))
            .unwrap();
        cx.update_window(handle, |_, window, cx| {
            window.click("extension-setup-back", cx)
        })
        .unwrap();
    }
    let failing = temporary.path().join("unavailable-diagnostics");
    std::fs::create_dir_all(&failing).unwrap();
    std::fs::write(failing.join(".mcp.json"),serde_json::json!({"mcpServers":{"unavailable":{"command":"node","args":["-e","process.exit(23)"]}}}).to_string()).unwrap();
    cx.update_window(handle, |_, window, cx| {
        window.click(("extensions-tab", 1usize), cx);
        view.update(cx, |v, cx| {
            v.extension_review_request(
                "extension_install_local",
                serde_json::json!({"path":failing}),
                cx,
            )
        });
    })
    .unwrap();
    wait_controls(&mut cx, "extensions-refresh");
    let failed_package = catalog
        .list()
        .unwrap()
        .into_iter()
        .find(|p| p.name == "unavailable-diagnostics")
        .unwrap();
    let failed_index = catalog
        .list()
        .unwrap()
        .iter()
        .position(|p| p.id == failed_package.id)
        .unwrap();
    cx.update_window(handle, |_, window, cx| {
        window.click(("extension-enable", failed_index), cx)
    })
    .unwrap();
    wait_controls(&mut cx, "extensions-refresh");
    cx.update_window(handle, |_, window, cx| {
        window.click(("extension-configure", failed_index * 1000), cx)
    })
    .unwrap();
    wait_controls(&mut cx, "extension-setup-save");
    cx.update_window(handle, |_, window, cx| {
        window.click("extension-setup-save", cx)
    })
    .unwrap();
    wait_controls(&mut cx, "extension-setup-verify");
    for attempt in 1..=2 {
        cx.update_window(handle, |_, window, cx| {
            window.click("extension-setup-verify", cx)
        })
        .unwrap();
        wait_controls(&mut cx, "extension-setup-verify");
        let failed = catalog
            .component_status(&failed_package.id, "mcpServers")
            .unwrap();
        assert_ne!(
            failed.readiness,
            xylo_services::extensions::Readiness::ExecutionVerified
        );
        assert!(failed.entries.iter().all(|e| e.evidence.is_none()));
        cx.capture_screenshot(handle)
            .unwrap()
            .save(output.join(format!("mcp-verification-failed-attempt-{attempt}.png")))
            .unwrap();
    }
    cx.update_window(handle, |_, window, cx| {
        window.click("extension-setup-back", cx);
        window.click(("extension-remove", failed_index), cx);
    })
    .unwrap();
    wait_controls(&mut cx, "extensions-refresh");
    let stalled = temporary.path().join("stalled-diagnostics");
    std::fs::create_dir_all(&stalled).unwrap();
    std::fs::write(stalled.join(".mcp.json"),serde_json::json!({"mcpServers":{"waiting":{"command":"node","args":["${CLAUDE_PLUGIN_ROOT}/stall.cjs"]}}}).to_string()).unwrap();
    std::fs::write(stalled.join("stall.cjs"),"require('node:fs').writeFileSync('cancel-mcp-started',String(process.pid));setInterval(()=>{},1000);").unwrap();
    cx.update_window(handle, |_, _, cx| {
        view.update(cx, |v, cx| {
            v.extension_review_request(
                "extension_install_local",
                serde_json::json!({"path":stalled}),
                cx,
            )
        })
    })
    .unwrap();
    wait_controls(&mut cx, "extensions-refresh");
    let stalled_package = catalog
        .list()
        .unwrap()
        .into_iter()
        .find(|p| p.name == "stalled-diagnostics")
        .unwrap();
    let stalled_index = catalog
        .list()
        .unwrap()
        .iter()
        .position(|p| p.id == stalled_package.id)
        .unwrap();
    cx.update_window(handle, |_, window, cx| {
        window.click(("extension-enable", stalled_index), cx)
    })
    .unwrap();
    wait_controls(&mut cx, "extensions-refresh");
    cx.update_window(handle, |_, window, cx| {
        window.click(("extension-configure", stalled_index * 1000), cx)
    })
    .unwrap();
    wait_controls(&mut cx, "extension-setup-save");
    cx.update_window(handle, |_, window, cx| {
        window.click("extension-setup-save", cx)
    })
    .unwrap();
    wait_controls(&mut cx, "extension-setup-verify");
    cx.update_window(handle, |_, window, cx| {
        window.click("extension-setup-verify", cx)
    })
    .unwrap();
    wait(&mut cx, &|_| project.join("cancel-mcp-started").exists());
    let child_pid = std::fs::read_to_string(project.join("cancel-mcp-started")).unwrap();
    cx.capture_screenshot(handle)
        .unwrap()
        .save(output.join("mcp-verification-cancellable.png"))
        .unwrap();
    cx.update_window(handle, |_, window, cx| {
        assert!(window.find("extension-cancel").visible());
        window.click("extension-cancel", cx);
        assert_eq!(
            window.find("extension-setup-save").disabled(),
            Some(true),
            "Cancel request must retain pending state until terminal event"
        );
    })
    .unwrap();
    wait_controls(&mut cx, "extension-setup-verify");
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("extension-cancel").is_none());
        assert!(window.find("extension-setup-back").visible());
    })
    .unwrap();
    let cancelled = catalog
        .component_status(&stalled_package.id, "mcpServers")
        .unwrap();
    assert_ne!(
        cancelled.readiness,
        xylo_services::extensions::Readiness::ExecutionVerified
    );
    assert!(
        cancelled
            .entries
            .iter()
            .all(|entry| entry.evidence.is_none())
    );
    let receipts = xylo_services::Store::open(temporary.path().join("profile/xylo.db"))
        .unwrap()
        .extension_execution_receipts(100)
        .unwrap();
    let receipt = receipts
        .iter()
        .find(|r| r["intent"]["package"] == stalled_package.id)
        .unwrap();
    assert_eq!(
        receipt["state"], "outcome_unknown",
        "Cancelled started work must preserve uncertain effects"
    );
    #[cfg(unix)]
    {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let alive = std::process::Command::new("/bin/kill")
                .args(["-0", child_pid.trim()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap()
                .success();
            if !alive {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Cancelled MCP descendant survived"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    cx.capture_screenshot(handle)
        .unwrap()
        .save(output.join("mcp-verification-cancelled.png"))
        .unwrap();
    cx.update_window(handle, |_, window, cx| {
        window.click("extension-setup-back", cx);
        window.click(("extension-remove", stalled_index), cx);
    })
    .unwrap();
    wait_controls(&mut cx, "extensions-refresh");
    cx.update_window(handle, |_, window, cx| {
        window.click(("extensions-tab", 1usize), cx);
        window.click(("extension-enable", 0usize), cx);
    })
    .unwrap();
    wait_controls(&mut cx, "extensions-refresh");
    assert!(!catalog.list().unwrap()[0].enabled);
    cx.update_window(handle, |_, window, cx| {
        window.click(("extension-remove", 0usize), cx)
    })
    .unwrap();
    wait_controls(&mut cx, "extensions-refresh");
    assert!(
        catalog.list().unwrap().is_empty(),
        "Actual Host must persist removal"
    );
    cx.update_window(handle, |_, window, cx| {
        window.click(("extensions-tab", 2usize), cx)
    })
    .unwrap();
    let initial_sources = catalog.sources().unwrap().len();
    for (source_input, revision, directory) in [
        ("openai/skills.git", "HEAD", ""),
        (
            "https://github.com/openai/skills/tree/main/skills/.curated",
            "main",
            "skills/.curated",
        ),
    ] {
        cx.update_window(handle, |_, window, cx| {
            let field = window.find_by_label("GitHub repository or directory URL");
            window.click(field.path().last().unwrap().clone(), cx);
            window.press(
                if cfg!(target_os = "macos") {
                    "cmd-a"
                } else {
                    "ctrl-a"
                },
                cx,
            );
            window.input(source_input, cx);
            window.click("extension-source-add", cx);
        })
        .unwrap();
        wait_controls(&mut cx, "extension-source-add");
        assert!(
            catalog
                .sources()
                .unwrap()
                .iter()
                .any(|s| s.repository == "openai/skills"
                    && s.revision == revision
                    && s.subdirectory == directory),
            "Native source input must persist normalized GitHub source"
        );
    }
    assert_eq!(catalog.sources().unwrap().len(), initial_sources + 2);
    for (attempt, source_input) in [
        ("duplicate", "https://github.com/openai/skills.git"),
        ("invalid", "https://not-github.invalid/owner/repo"),
    ] {
        cx.update_window(handle, |_, window, cx| {
            let field = window.find_by_label("GitHub repository or directory URL");
            window.click(field.path().last().unwrap().clone(), cx);
            window.press(
                if cfg!(target_os = "macos") {
                    "cmd-a"
                } else {
                    "ctrl-a"
                },
                cx,
            );
            window.input(source_input, cx);
            window.click("extension-source-add", cx);
        })
        .unwrap();
        wait_controls(&mut cx, "extension-source-add");
        assert_eq!(
            catalog.sources().unwrap().len(),
            initial_sources + 2,
            "Rejected source must not mutate catalog"
        );
        cx.capture_screenshot(handle)
            .unwrap()
            .save(output.join(format!("source-add-{attempt}.png")))
            .unwrap();
    }
    for index in [initial_sources + 1, initial_sources] {
        cx.update_window(handle, |_, window, cx| {
            window.click(("extension-source-remove", index), cx)
        })
        .unwrap();
        wait_controls(&mut cx, "extension-source-add");
    }
    assert_eq!(
        catalog.sources().unwrap().len(),
        initial_sources,
        "Native remove must persist source cleanup"
    );
    std::fs::write(output.join("lifecycle.json"),serde_json::to_vec_pretty(&serde_json::json!({"platform":"macOS native offscreen GPU","local_install":true,"package_id":package_id,"enable":true,"disable":true,"remove":true,"discovery_network_requested":false,"physical_desktop_verified":false})).unwrap()).unwrap();
    client.shutdown().unwrap();
    println!(
        "Portable extension lifecycle and captures passed: {}",
        output.display()
    );
}
