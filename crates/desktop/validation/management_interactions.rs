//! Real native controls, durable helper registries and isolated native clients.
//! No network, container daemon, account, permission dialog or device is used.
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
    mod management_tests {
        use super::{
            App, AppContext, AppSettings, Bounds, ContainerRuntime, ContainerTarget, Entity,
            EnvironmentProfile, EnvironmentTarget, HostClient, NativePluginManifest, Page,
            PluginBudgets, TestAppContext, Uuid, WindowBounds, WindowOptions, Workbench, px, size,
        };
        use gpui_kit::{
            AnyWindowHandle,
            test::{TestQueryExt, TestWindowExt},
        };
        use sha2::{Digest, Sha256};
        use std::{
            path::{Path, PathBuf},
            time::{Duration, Instant},
        };
        struct Fixture {
            directory: tempfile::TempDir,
            base: PathBuf,
            workspace: PathBuf,
            paths: workbench_runtime::AppPaths,
            host: HostClient,
            window: AnyWindowHandle,
            view: Entity<Workbench>,
        }
        fn wait(
            cx: &mut TestAppContext,
            view: &Entity<Workbench>,
            condition: impl Fn(&Workbench, &App) -> bool,
        ) {
            let deadline = Instant::now() + Duration::from_secs(25);
            loop {
                cx.executor().advance_clock(Duration::from_millis(25));
                cx.run_until_parked();
                if cx.update(|cx| condition(view.read(cx), cx)) {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "Native management timed out: {}",
                    cx.update(|cx| {
                        let view = view.read(cx);
                        format!(
                            "plugin={},environment={},error={:?}",
                            view.plugins.notice,
                            view.validation_environment_notice(),
                            view.error
                        )
                    })
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        fn open(cx: &mut TestAppContext) -> Fixture {
            open_at_page(cx, None)
        }
        fn open_at_page(cx: &mut TestAppContext, initial_page: Option<Page>) -> Fixture {
            cx.executor().allow_parking();
            let directory = tempfile::tempdir().unwrap();
            let base = directory.path().canonicalize().unwrap();
            let workspace = base.join("project");
            std::fs::create_dir(&workspace).unwrap();
            let paths = workbench_runtime::AppPaths::at(base.join("profile")).unwrap();
            let store = workbench_services::Store::open(&paths.database).unwrap();
            let mut settings = AppSettings {
                last_workspace: initial_page.is_none().then(|| workspace.clone()),
                ..Default::default()
            };
            if let Some(page) = initial_page {
                settings.ui.active_page = page.key().to_owned();
            }
            store.save_settings(&settings).unwrap();
            let host = HostClient::spawn_at(paths.clone()).unwrap();
            let client = host.0.clone();
            cx.update(gpui_kit::init);
            let (window, view) = cx.update(|cx| {
                gpui_kit::open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds {
                            origin: Default::default(),
                            size: size(px(1500.), px(2000.)),
                        })),
                        ..Default::default()
                    },
                    cx,
                    |window, cx| cx.new(|cx| Workbench::new(Ok(host), window, cx)),
                )
                .unwrap()
            });
            wait(cx, &view, |view, _| {
                view.settings_loaded && (view.workspace_ready || initial_page.is_some())
            });
            Fixture {
                directory,
                base,
                workspace,
                paths,
                host: client,
                window,
                view,
            }
        }
        fn shutdown(cx: &mut TestAppContext, fixture: &Fixture) {
            fixture.host.shutdown().unwrap();
            let lock = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(fixture.paths.data.join("host.lock"))
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                cx.run_until_parked();
                if lock.try_lock().is_ok() {
                    break;
                }
                assert!(Instant::now() < deadline, "Host fixture did not shut down");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        fn inspect(cx: &mut TestAppContext, fixture: &Fixture, path: PathBuf) {
            cx.update_window(fixture.window, |_, _, cx| {
                fixture
                    .view
                    .update(cx, |view, cx| view.plugin_inspect(path, cx))
            })
            .unwrap();
            wait(cx, &fixture.view, |view, _| {
                view.validation_plugin_review().is_some() && view.validation_plugin_idle()
            });
        }
        fn select_plugin(cx: &mut TestAppContext, fixture: &Fixture, id: &str) {
            let index = cx.update(|cx| {
                fixture
                    .view
                    .read(cx)
                    .validation_plugin_catalog()
                    .unwrap()
                    .plugins
                    .iter()
                    .position(|plugin| plugin.manifest.id == id)
                    .unwrap()
            });
            cx.update_window(fixture.window, |_, window, cx| {
                window.render_frame(cx);
                window.click(("plugin-select", index), cx);
            })
            .unwrap();
            cx.run_until_parked();
        }
        fn package(base: &Path, id: &str, source: &str, permissions: &[&str]) -> PathBuf {
            let root = base.join(id);
            std::fs::create_dir(&root).unwrap();
            std::fs::write(root.join("component.wat"), source).unwrap();
            let manifest = NativePluginManifest {
                format: 1,
                id: id.into(),
                name: id.into(),
                version: "1.0.0".into(),
                entrypoint: "component.wat".into(),
                sha256: format!("{:x}", Sha256::digest(source.as_bytes())),
                capabilities: permissions.iter().map(|value| (*value).into()).collect(),
                budgets: PluginBudgets::default(),
            };
            let path = root.join("plugin.json");
            std::fs::write(&path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
            path
        }
        #[gpui_kit::test]
        fn restored_global_management_pages_load_without_a_project(cx: &mut TestAppContext) {
            for page in [Page::Plugins, Page::Environments] {
                let fixture = open_at_page(cx, Some(page));
                wait(cx, &fixture.view, |view, _| match page {
                    Page::Plugins => {
                        view.validation_plugin_catalog().is_some() && view.validation_plugin_idle()
                    }
                    Page::Environments => {
                        view.validation_environment_loaded() && view.validation_environment_idle()
                    }
                    _ => unreachable!(),
                });
                assert!(cx.update(|cx| fixture.view.read(cx).workspace.is_none()));
                shutdown(cx, &fixture);
            }
        }
        fn click(cx: &mut TestAppContext, fixture: &Fixture, id: &str) {
            cx.update_window(fixture.window, |_, window, cx| {
                window.render_frame(cx);
                window.click(id.to_owned(), cx);
            })
            .unwrap();
            cx.run_until_parked();
        }
        fn select_environment(cx: &mut TestAppContext, fixture: &Fixture, id: Uuid) {
            let index = cx.update(|cx| {
                fixture
                    .view
                    .read(cx)
                    .validation_environment_records()
                    .iter()
                    .position(|record| record.profile.id == id)
                    .unwrap()
            });
            cx.update_window(fixture.window, |_, window, cx| {
                window.render_frame(cx);
                window.click(("environment-select", index), cx);
            })
            .unwrap();
            cx.run_until_parked();
        }
        #[gpui_kit::test]
        fn shipped_echo_installs_enables_runs_and_restores_without_auto_run(
            cx: &mut TestAppContext,
        ) {
            let fixture = open(cx);
            let root = fixture.base.join("echo-package");
            std::fs::create_dir(&root).unwrap();
            std::fs::write(
                root.join("echo.wat"),
                include_bytes!("../../../examples/plugins/echo/echo.wat"),
            )
            .unwrap();
            std::fs::write(
                root.join("plugin.json"),
                include_bytes!("../../../examples/plugins/echo/plugin.json"),
            )
            .unwrap();
            cx.update_window(fixture.window, |_, window, cx| {
                fixture
                    .view
                    .update(cx, |view, cx| view.navigate(Page::Plugins, cx));
                window.render_frame(cx);
            })
            .unwrap();
            wait(cx, &fixture.view, |view, _| {
                view.validation_plugin_catalog().is_some() && view.validation_plugin_idle()
            });
            inspect(cx, &fixture, root.join("plugin.json"));
            cx.update_window(fixture.window, |_, window, cx| {
                window.render_frame(cx);
                assert_eq!(window.find("plugin-enable-install").checked(), Some(false));
                window.click("plugin-enable-install", cx);
                window.render_frame(cx);
                window.click("plugin-install", cx);
            })
            .unwrap();
            wait(cx, &fixture.view, |view, _| {
                view.validation_plugin_idle()
                    && view.validation_plugin_catalog().is_some_and(|catalog| {
                        catalog.plugins.iter().any(|plugin| {
                            plugin.manifest.id == "org.agentworkbench.echo" && plugin.enabled
                        })
                    })
            });
            select_plugin(cx, &fixture, "org.agentworkbench.echo");
            cx.update_window(fixture.window, |_, window, cx| {
                fixture.view.update(cx, |view, cx| {
                    view.plugins
                        .input
                        .update(cx, |input, cx| input.focus(window, cx))
                });
                window.render_frame(cx);
                window.input("Native shipped example", cx);
                window.render_frame(cx);
                window.click("plugin-run", cx);
            })
            .unwrap();
            wait(cx, &fixture.view, |view, cx| {
                view.validation_plugin_idle()
                    && view.validation_plugin_output(cx) == "Native shipped example"
            });
            cx.update_window(fixture.window, |_, window, cx| {
                fixture.view.update(cx, |view, cx| {
                    view.plugins = super::plugins::PluginUi::new(window, cx);
                    view.navigate(Page::Plugins, cx);
                });
            })
            .unwrap();
            wait(cx, &fixture.view, |view, _| {
                view.validation_plugin_catalog().is_some_and(|catalog| {
                    catalog.plugins.iter().any(|plugin| {
                        plugin.manifest.id == "org.agentworkbench.echo" && plugin.enabled
                    })
                }) && view.validation_plugin_idle()
            });
            assert!(
                cx.update(|cx| fixture
                    .view
                    .read(cx)
                    .validation_plugin_output(cx)
                    .is_empty()),
                "Restoring installed state must not execute the plugin"
            );
            shutdown(cx, &fixture);
        }
        #[gpui_kit::test]
        fn reviewed_permissions_revoke_real_reads_and_writer_returns_artifact(
            cx: &mut TestAppContext,
        ) {
            let fixture = open(cx);
            std::fs::write(
                fixture.workspace.join("read-me.txt"),
                "Only the granted reader can read this",
            )
            .unwrap();
            let reader = package(
                &fixture.base,
                "test.reader",
                include_str!("../../capabilities/src/plugins/fixtures/reader.wat"),
                &["read_workspace"],
            );
            cx.update_window(fixture.window, |_, _, cx| {
                fixture
                    .view
                    .update(cx, |view, cx| view.navigate(Page::Plugins, cx))
            })
            .unwrap();
            wait(cx, &fixture.view, |view, _| {
                view.validation_plugin_catalog().is_some() && view.validation_plugin_idle()
            });
            inspect(cx, &fixture, reader);
            cx.update_window(fixture.window, |_, window, cx| {
                window.render_frame(cx);
                window.click(("plugin-review-grant", 0usize), cx);
                window.click("plugin-enable-install", cx);
                window.render_frame(cx);
                window.click("plugin-install", cx);
            })
            .unwrap();
            wait(cx, &fixture.view, |view, _| {
                view.validation_plugin_idle()
                    && view.validation_plugin_catalog().is_some_and(|catalog| {
                        catalog.plugins.iter().any(|plugin| {
                            plugin.manifest.id == "test.reader"
                                && plugin.grants == ["read_workspace"]
                        })
                    })
            });
            select_plugin(cx, &fixture, "test.reader");
            cx.update_window(fixture.window, |_, window, cx| {
                fixture.view.update(cx, |view, cx| {
                    view.plugins
                        .input
                        .update(cx, |input, cx| input.set_value("read-me.txt", window, cx))
                });
                window.render_frame(cx);
                window.click("plugin-run", cx);
            })
            .unwrap();
            wait(cx, &fixture.view, |view, cx| {
                view.validation_plugin_idle()
                    && view.validation_plugin_output(cx) == "Only the granted reader can read this"
            });
            cx.update_window(fixture.window, |_, window, cx| {
                window.render_frame(cx);
                window.click("plugin-revoke", cx);
            })
            .unwrap();
            cx.run_until_parked();
            assert!(cx.has_pending_prompt());
            cx.simulate_prompt_answer("Revoke permissions");
            wait(cx, &fixture.view, |view, _| {
                view.validation_plugin_idle()
                    && view.validation_plugin_catalog().is_some_and(|catalog| {
                        catalog
                            .plugins
                            .iter()
                            .find(|plugin| plugin.manifest.id == "test.reader")
                            .is_some_and(|plugin| plugin.grants.is_empty())
                    })
            });
            cx.update_window(fixture.window, |_, window, cx| {
                window.render_frame(cx);
                window.click("plugin-run", cx);
            })
            .unwrap();
            wait(cx, &fixture.view, |view, cx| {
                view.validation_plugin_idle()
                    && view.validation_plugin_output(cx).contains("permissions")
            });
            let writer = package(
                &fixture.base,
                "test.writer",
                include_str!("../../capabilities/src/plugins/fixtures/writer.wat"),
                &["write_artifacts"],
            );
            inspect(cx, &fixture, writer);
            cx.update_window(fixture.window, |_, window, cx| {
                window.render_frame(cx);
                window.click(("plugin-review-grant", 0usize), cx);
                window.click("plugin-enable-install", cx);
                window.render_frame(cx);
                window.click("plugin-install", cx);
            })
            .unwrap();
            wait(cx, &fixture.view, |view, _| {
                view.validation_plugin_idle()
                    && view
                        .validation_plugin_catalog()
                        .is_some_and(|catalog| catalog.plugins.len() == 2)
            });
            select_plugin(cx, &fixture, "test.writer");
            cx.update_window(fixture.window, |_, window, cx| {
                fixture.view.update(cx, |view, cx| {
                    view.plugins.input.update(cx, |input, cx| {
                        input.set_value("Native plugin artifact", window, cx)
                    })
                });
                window.render_frame(cx);
                window.click("plugin-run", cx);
            })
            .unwrap();
            wait(cx, &fixture.view, |view, _| {
                view.validation_plugin_idle() && !view.validation_plugin_artifacts().is_empty()
            });
            let artifacts = cx.update(|cx| fixture.view.read(cx).validation_plugin_artifacts());
            assert_eq!(
                std::fs::read_to_string(&artifacts[0]).unwrap(),
                "Native plugin artifact"
            );
            shutdown(cx, &fixture);
        }
        #[cfg(unix)]
        fn fake_ssh(base: &Path) -> PathBuf {
            use std::os::unix::fs::PermissionsExt;
            let path = base.join("fixture-ssh");
            let root = base.to_string_lossy().replace('\'', "'\\''");
            let script = r#"#!/bin/sh
set -eu
root='ROOT'
printf '<%s>\n' "$@" >> "$root/ssh-arguments"
socket=''; control=''; master=no; previous=''
for arg in "$@"; do
 case "$previous" in -S) socket=$arg;; -O) control=$arg;; esac
 [ "$arg" != '-M' ] || master=yes
 previous=$arg
done
if [ "$master" = yes ]; then
 printf '%s' "$$" > "$socket"
 trap 'rm -f "$socket"; exit 0' TERM INT EXIT
 while :; do sleep 1; done
fi
if [ "$control" = check ]; then [ -f "$socket" ] && kill -0 "$(cat "$socket")" 2>/dev/null; exit $?; fi
if [ "$control" = exit ]; then
 if [ -f "$socket" ]; then kill -TERM "$(cat "$socket")"; rm -f "$socket"; fi
 exit 0
fi
[ -f "$socket" ] || exit 255
case "$*" in
 *__large_output__*) head -c 20000 /dev/zero | tr '\000' 'x'; printf 'retained stderr' >&2; exit 0;;
esac
printf 'isolated remote command completed'
"#;
            std::fs::write(&path, script.replace("ROOT", &root)).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            path
        }
        #[cfg(unix)]
        fn fake_container(base: &Path) -> PathBuf {
            use std::os::unix::fs::PermissionsExt;
            let path = base.join("fixture-container");
            let root = format!("'{}'", base.to_string_lossy().replace('\'', "'\\''"));
            std::fs::write(
                &path,
                include_str!("fixtures/management-container.sh").replace("ROOT", &root),
            )
            .unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            path
        }
        #[cfg(unix)]
        #[gpui_kit::test]
        fn managed_container_cancel_restores_original_receipt_without_replaying_and_removal_confirms(
            cx: &mut TestAppContext,
        ) {
            let fixture = open(cx);
            let id = Uuid::new_v4();
            let profile = EnvironmentProfile {
                id,
                name: "Isolated container".into(),
                target: EnvironmentTarget::Container {
                    executable: fake_container(&fixture.base),
                    runtime: ContainerRuntime::Docker,
                    container: ContainerTarget::Managed {
                        image: "local:fixture".into(),
                        command: vec!["sleep".into(), "infinity".into()],
                    },
                },
            };
            cx.update_window(fixture.window, |_, _, cx| {
                fixture
                    .view
                    .update(cx, |view, cx| view.navigate(Page::Environments, cx));
            })
            .unwrap();
            wait(cx, &fixture.view, |view, _| {
                view.validation_environment_idle()
            });
            cx.update_window(fixture.window, |_, window, cx| {
                fixture.view.update(cx, |view, cx| {
                    view.validation_environment_setup(profile, window, cx)
                });
            })
            .unwrap();
            click(cx, &fixture, "environment-save");
            wait(cx, &fixture.view, |view, _| {
                view.validation_environment_idle()
                    && view
                        .validation_environment_records()
                        .iter()
                        .any(|record| record.profile.id == id)
            });
            assert!(!fixture.base.join("container-arguments").exists());
            for (action, revision) in [
                ("environment-create", 2),
                ("environment-start", 3),
                ("environment-connect", 4),
            ] {
                click(cx, &fixture, action);
                wait(cx, &fixture.view, |view, _| {
                    view.validation_environment_idle()
                        && view
                            .validation_environment_records()
                            .iter()
                            .any(|record| record.profile.id == id && record.revision == revision)
                });
            }
            cx.update_window(fixture.window, |_, window, cx| {
                fixture.view.update(cx, |view, cx| {
                    view.environments
                        .command
                        .update(cx, |input, cx| input.set_value("__hang__", window, cx))
                });
                window.render_frame(cx);
                window.click("environment-run", cx);
            })
            .unwrap();
            wait(cx, &fixture.view, |_, _| {
                fixture.base.join("dispatched").exists()
            });
            click(cx, &fixture, "environment-cancel");
            wait(cx, &fixture.view, |view, _| {
                view.validation_environment_idle()
                    && !view.validation_environment_operations().is_empty()
            });
            let operation = cx.update(|cx| {
                fixture.view.read(cx).validation_environment_operations()[0].operation_id
            });
            assert_eq!(
                std::fs::read_to_string(fixture.base.join("executions"))
                    .unwrap()
                    .lines()
                    .count(),
                1
            );
            // Recreate the controller to prove pending IDs come from durable receipts,
            // rather than a process-local retry or a newly generated operation.
            cx.update_window(fixture.window, |_, window, cx| {
                fixture.view.update(cx, |view, cx| {
                    view.environments = super::environments::EnvironmentUi::new(window, cx);
                    view.subscribe_environments(cx);
                    view.navigate(Page::Environments, cx);
                });
            })
            .unwrap();
            wait(cx, &fixture.view, |view, _| {
                view.validation_environment_idle()
                    && view
                        .validation_environment_operations()
                        .iter()
                        .any(|saved| saved.operation_id == operation)
            });
            select_environment(cx, &fixture, id);
            cx.update_window(fixture.window, |_, window, cx| {
                window.render_frame(cx);
                assert_eq!(window.find("environment-run").disabled(), Some(true));
                window.click(("environment-check-operation", 0usize), cx);
            })
            .unwrap();
            wait(cx, &fixture.view, |view, _| {
                view.validation_environment_idle()
            });
            cx.update_window(fixture.window, |_, window, cx| {
                window.render_frame(cx);
                window.click(("environment-record-operation", 0usize), cx);
                fixture.view.update(cx, |view, cx| {
                    view.validation_environment_note(
                        "Inspected isolated target; command did not complete.",
                        window,
                        cx,
                    )
                });
                window.render_frame(cx);
                window.click("environment-resolve-failed", cx);
            })
            .unwrap();
            cx.run_until_parked();
            assert!(cx.has_pending_prompt());
            cx.simulate_prompt_answer("Record result");
            wait(cx, &fixture.view, |view, _| {
                view.validation_environment_idle()
                    && view.validation_environment_operations().is_empty()
            });
            assert_eq!(
                std::fs::read_to_string(fixture.base.join("executions"))
                    .unwrap()
                    .lines()
                    .count(),
                1,
                "Checking/recording never replays the command"
            );
            for action in ["environment-disconnect", "environment-stop"] {
                click(cx, &fixture, action);
                wait(cx, &fixture.view, |view, _| {
                    view.validation_environment_idle()
                });
            }
            click(cx, &fixture, "environment-remove-container");
            assert!(cx.has_pending_prompt());
            cx.simulate_prompt_answer("Cancel");
            cx.run_until_parked();
            assert!(fixture.base.join("exists").exists());
            click(cx, &fixture, "environment-remove-container");
            cx.simulate_prompt_answer("Remove container");
            wait(cx, &fixture.view, |view, _| {
                view.validation_environment_idle()
                    && view
                        .validation_environment_records()
                        .iter()
                        .any(|record| record.profile.id == id && record.container_id.is_none())
            });
            assert!(!fixture.base.join("exists").exists());
            click(cx, &fixture, "environment-remove");
            cx.simulate_prompt_answer("Remove profile");
            wait(cx, &fixture.view, |view, _| {
                view.validation_environment_idle()
                    && view.validation_environment_records().is_empty()
            });
            let arguments =
                std::fs::read_to_string(fixture.base.join("container-arguments")).unwrap();
            assert!(!arguments.contains("<--force>"));
            assert!(!arguments.contains("<--volumes>"));
            shutdown(cx, &fixture);
        }
        #[cfg(unix)]
        #[gpui_kit::test]
        fn saved_ssh_profile_connects_runs_disconnects_and_restores_without_network(
            cx: &mut TestAppContext,
        ) {
            let fixture = open(cx);
            let executable = fake_ssh(&fixture.base);
            let known = fixture.base.join("verified-hosts");
            std::fs::write(&known, "fixture.invalid ssh-ed25519 fixture\n").unwrap();
            let id = Uuid::new_v4();
            let profile = EnvironmentProfile {
                id,
                name: "Isolated SSH".into(),
                target: EnvironmentTarget::Ssh {
                    executable,
                    host: "fixture.invalid".into(),
                    user: "fixture".into(),
                    port: 22,
                    known_hosts: known,
                    identity_file: None,
                },
            };
            cx.update_window(fixture.window, |_, window, cx| {
                fixture.view.update(cx, |view, cx| {
                    view.navigate(Page::Environments, cx);
                    view.validation_environment_setup(profile, window, cx);
                })
            })
            .unwrap();
            wait(cx, &fixture.view, |view, _| {
                view.validation_environment_idle()
            });
            cx.update_window(fixture.window, |_, window, cx| {
                window.render_frame(cx);
                window.click("environment-save", cx);
            })
            .unwrap();
            wait(cx, &fixture.view, |view, _| {
                view.validation_environment_idle()
                    && view
                        .validation_environment_records()
                        .iter()
                        .any(|record| record.profile.id == id)
            });
            assert!(
                !fixture.base.join("ssh-arguments").exists(),
                "Saving must not start SSH"
            );
            cx.update_window(fixture.window, |_, window, cx| {
                window.render_frame(cx);
                window.click("environment-connect", cx);
            })
            .unwrap();
            wait(cx, &fixture.view, |view, _| {
                view.validation_environment_idle()
                    && view
                        .validation_environment_records()
                        .iter()
                        .any(|record| record.profile.id == id && record.connected)
            });
            cx.update_window(fixture.window, |_, window, cx| {
                fixture.view.update(cx, |view, cx| {
                    view.environments.command.update(cx, |input, cx| {
                        input.set_value("printf 'literal $(must-not-run)'", window, cx)
                    })
                });
                window.render_frame(cx);
                window.click("environment-run", cx);
            })
            .unwrap();
            wait(cx, &fixture.view, |view, cx| {
                view.validation_environment_idle()
                    && view
                        .validation_environment_output(cx)
                        .contains("isolated remote command completed")
            });
            let output = cx.update(|cx| {
                fixture
                    .view
                    .read(cx)
                    .validation_environment_output_details()
                    .expect("Native command output")
            });
            assert_eq!(
                output.artifacts.len(),
                2,
                "Both actual raw streams must be retained"
            );
            for artifact in &output.artifacts {
                let bytes = std::fs::read(&artifact.path).unwrap();
                assert_eq!(format!("{:x}", Sha256::digest(&bytes)), artifact.sha256);
                assert_eq!(bytes.len() as u64, artifact.stored_bytes);
                assert!(
                    artifact.complete,
                    "Normal EOF must retain the complete stream"
                );
                assert!(
                    cx.update(|cx| fixture.view.read(cx).artifacts.contains(&artifact.path)),
                    "Native result must route its saved file into Artifacts"
                );
            }
            cx.update_window(fixture.window, |_, window, cx| {
                window.render_frame(cx);
                assert!(window.try_find_by_label("Open standard output").is_some());
                assert!(window.try_find_by_label("Open standard error").is_some());
            })
            .unwrap();
            assert!(!fixture.workspace.join("must-not-run").exists());
            cx.update_window(fixture.window, |_, window, cx| {
                fixture.view.update(cx, |view, cx| {
                    view.environments.command.update(cx, |input, cx| {
                        input.set_value("__large_output__", window, cx)
                    });
                });
                window.render_frame(cx);
                window.click("environment-run", cx);
            })
            .unwrap();
            wait(cx, &fixture.view, |view, _| {
                view.validation_environment_idle()
                    && view
                        .validation_environment_output_details()
                        .is_some_and(|output| output.truncated)
            });
            let large = cx.update(|cx| {
                fixture
                    .view
                    .read(cx)
                    .validation_environment_output_details()
                    .unwrap()
            });
            let stdout = large
                .artifacts
                .iter()
                .find(|artifact| artifact.stream == super::EnvironmentOutputStream::Stdout)
                .unwrap();
            assert_eq!(stdout.stored_bytes, 20000);
            assert_eq!(stdout.observed_bytes, 20000);
            assert!(stdout.complete);
            assert_eq!(std::fs::read(&stdout.path).unwrap(), vec![b'x'; 20000]);
            cx.update_window(fixture.window, |_, window, cx| {
                window.render_frame(cx);
                assert!(window.try_find_by_label("Open standard output").is_some());
                assert!(window.try_find_by_label("Open standard error").is_some());
            })
            .unwrap();
            cx.update_window(fixture.window, |_, window, cx| {
                window.render_frame(cx);
                window.click("environment-disconnect", cx);
            })
            .unwrap();
            wait(cx, &fixture.view, |view, _| {
                view.validation_environment_idle()
                    && view
                        .validation_environment_records()
                        .iter()
                        .any(|record| record.profile.id == id && !record.connected)
            });
            let before = std::fs::read(fixture.base.join("ssh-arguments")).unwrap();
            cx.update_window(fixture.window, |_, window, cx| {
                fixture.view.update(cx, |view, cx| {
                    view.environments = super::environments::EnvironmentUi::new(window, cx);
                    view.subscribe_environments(cx);
                    view.navigate(Page::Environments, cx);
                })
            })
            .unwrap();
            wait(cx, &fixture.view, |view, _| {
                view.validation_environment_idle()
                    && view
                        .validation_environment_records()
                        .iter()
                        .any(|record| record.profile.id == id && !record.connected)
            });
            assert_eq!(
                std::fs::read(fixture.base.join("ssh-arguments")).unwrap(),
                before,
                "Restoring profiles must not connect or execute"
            );
            cx.update_window(fixture.window, |_, window, cx| {
                window.render_frame(cx);
                window.click(("environment-select", 0usize), cx);
            })
            .unwrap();
            assert_eq!(
                std::fs::read(fixture.base.join("ssh-arguments")).unwrap(),
                before,
                "Selecting a saved profile must not connect or execute"
            );
            click(cx, &fixture, "environment-saved-results");
            wait(cx, &fixture.view, |view, _| {
                view.validation_environment_idle()
                    && view.validation_environment_history().len() >= 2
            });
            let history = cx.update(|cx| fixture.view.read(cx).validation_environment_history());
            let index = history
                .iter()
                .position(|receipt| receipt.command_label.as_deref() == Some("__large_output__"))
                .expect("Saved command label");
            cx.update_window(fixture.window, |_, window, cx| {
                window.render_frame(cx);
                window.click(("environment-open-saved", index), cx);
            })
            .unwrap();
            wait(cx, &fixture.view, |view, _| {
                view.validation_environment_idle()
                    && view
                        .validation_environment_output_details()
                        .is_some_and(|output| {
                            output
                                .artifacts
                                .iter()
                                .any(|artifact| artifact.stored_bytes == 20000)
                        })
            });
            assert_eq!(
                std::fs::read(fixture.base.join("ssh-arguments")).unwrap(),
                before,
                "Loading saved command output must not start the native client or replay the command"
            );
            shutdown(cx, &fixture);
        }
        #[cfg(unix)]
        #[gpui_kit::test]
        fn delayed_profile_save_keeps_new_typing_and_blocks_scope_and_quit_until_ack(
            cx: &mut TestAppContext,
        ) {
            let fixture = open(cx);
            let known = fixture.base.join("verified-hosts");
            std::fs::write(&known, "fixture.invalid ssh-ed25519 fixture\n").unwrap();
            let id = Uuid::new_v4();
            let profile = EnvironmentProfile {
                id,
                name: "Submitted name".into(),
                target: EnvironmentTarget::Ssh {
                    executable: fake_ssh(&fixture.base),
                    host: "fixture.invalid".into(),
                    user: "fixture".into(),
                    port: 22,
                    known_hosts: known,
                    identity_file: None,
                },
            };
            cx.update_window(fixture.window, |_, _, cx| {
                fixture
                    .view
                    .update(cx, |view, cx| view.navigate(Page::Environments, cx));
            })
            .unwrap();
            wait(cx, &fixture.view, |view, _| {
                view.validation_environment_idle()
            });
            let lease = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(
                    fixture
                        .paths
                        .data
                        .join("native-management/environments/leases")
                        .join(format!("{id}.lock")),
                )
                .unwrap();
            lease.lock().unwrap();
            cx.update_window(fixture.window, |_, window, cx| {
                fixture.view.update(cx, |view, cx| {
                    view.validation_environment_setup(profile, window, cx)
                });
                window.render_frame(cx);
                window.click("environment-save", cx);
                fixture.view.update(cx, |view, cx| {
                    view.validation_environment_focus_name(window, cx)
                });
                window.render_frame(cx);
                window.press("cmd-a", cx);
                window.input("Newer form name", cx);
            })
            .unwrap();
            cx.run_until_parked();
            let other = fixture.base.join("other-project");
            std::fs::create_dir(&other).unwrap();
            cx.update_window(fixture.window, |_, window, cx| {
                fixture.view.update(cx, |view, cx| {
                    assert!(!view.validation_environment_idle());
                    view.set_workspace(other.clone(), window, cx);
                    assert_eq!(view.workspace.as_ref(), Some(&fixture.workspace));
                    view.request_quit(window, cx);
                    assert!(!view.quitting);
                });
            })
            .unwrap();
            lease.unlock().unwrap();
            wait(cx, &fixture.view, |view, _| {
                view.validation_environment_idle()
                    && view
                        .validation_environment_records()
                        .iter()
                        .any(|record| record.profile.id == id)
            });
            cx.update(|cx| {
                let view = fixture.view.read(cx);
                assert_eq!(view.validation_environment_name(cx), "Newer form name");
                assert!(view.validation_environment_dirty());
                assert_eq!(
                    view.validation_environment_records()[0].profile.name,
                    "Submitted name"
                );
            });
            // Cancel a second Save while admission waits for the same profile.
            // Checking its exact context must durably fence it as undispatched,
            // preserve this form, and make a later explicit Save possible.
            lease.lock().unwrap();
            click(cx, &fixture, "environment-save");
            let operation_leases = fixture
                .paths
                .data
                .join("native-management/environments/operation-leases");
            wait(cx, &fixture.view, |_, _| {
                std::fs::read_dir(&operation_leases).unwrap().count() >= 2
            });
            click(cx, &fixture, "environment-cancel");
            wait(cx, &fixture.view, |view, _| {
                view.validation_environment_idle()
                    && !view.validation_environment_operations().is_empty()
            });
            lease.unlock().unwrap();
            cx.update_window(fixture.window, |_, window, cx| {
                window.render_frame(cx);
                window.click(("environment-check-operation", 0usize), cx);
            })
            .unwrap();
            wait(cx, &fixture.view, |view, _| {
                view.validation_environment_idle()
                    && view.validation_environment_operations().is_empty()
            });
            cx.update(|cx| {
                let view = fixture.view.read(cx);
                assert_eq!(view.validation_environment_name(cx), "Newer form name");
                assert!(view.validation_environment_dirty());
                assert_eq!(
                    view.validation_environment_records()[0].profile.name,
                    "Submitted name"
                );
            });
            click(cx, &fixture, "environment-save");
            wait(cx, &fixture.view, |view, _| {
                view.validation_environment_idle()
                    && !view.validation_environment_dirty()
                    && view
                        .validation_environment_records()
                        .iter()
                        .any(|record| record.profile.name == "Newer form name")
            });
            let arguments = std::fs::read_to_string(fixture.base.join("ssh-arguments")).unwrap();
            assert!(
                arguments.contains("<-O>\n<check>"),
                "An existing Unix profile may check its local master before editing"
            );
            assert!(
                !arguments.contains("<-M>"),
                "Saving must never start a session"
            );
            assert!(
                !arguments.contains("<exec "),
                "Saving must never execute a remote command"
            );
            cx.update_window(fixture.window, |_, window, cx| {
                fixture
                    .view
                    .update(cx, |view, cx| view.set_workspace(other.clone(), window, cx));
            })
            .unwrap();
            wait(cx, &fixture.view, |view, _| {
                view.workspace_ready && view.workspace.as_ref() == Some(&other)
            });
            assert_eq!(
                cx.update(|cx| fixture.view.read(cx).validation_environment_name(cx)),
                "Newer form name"
            );
            shutdown(cx, &fixture);
        }
        #[gpui_kit::test]
        fn stale_feature_results_do_not_overwrite_product_messages_and_close_keeps_setup(
            cx: &mut TestAppContext,
        ) {
            let fixture = open(cx);
            cx.update_window(fixture.window, |_, window, cx| {
                fixture.view.update(cx, |view, cx| {
                    view.error = None;
                    view.native.message = "Keep current result".into();
                    for name in [
                        "helper_plugin_list",
                        "plugin_run_installed",
                        "helper_environment_list",
                        "environment_execute",
                    ] {
                        view.capability_result(
                            Uuid::new_v4(),
                            name,
                            Err("Retired request".into()),
                            window,
                            cx,
                        );
                    }
                    assert!(view.error.is_none());
                    assert_eq!(view.native.message, "Keep current result");
                    view.navigate(Page::Environments, cx);
                })
            })
            .unwrap();
            wait(cx, &fixture.view, |view, _| {
                view.validation_environment_idle()
            });
            cx.update_window(fixture.window, |_, window, cx| {
                fixture.view.update(cx, |view, cx| {
                    view.validation_environment_focus_name(window, cx)
                });
                window.render_frame(cx);
                window.input("Unsaved setup", cx);
            })
            .unwrap();
            cx.run_until_parked();
            assert!(cx.update(|cx| fixture.view.read(cx).validation_environment_dirty()));
            cx.update_window(fixture.window, |_, window, cx| {
                fixture
                    .view
                    .update(cx, |view, cx| view.request_quit(window, cx))
            })
            .unwrap();
            cx.run_until_parked();
            assert!(cx.has_pending_prompt());
            cx.simulate_prompt_answer("Keep editing");
            cx.run_until_parked();
            assert!(cx.update(|cx| !fixture.view.read(cx).quitting));
            assert_eq!(
                cx.update(|cx| fixture.view.read(cx).validation_environment_name(cx)),
                "Unsaved setup"
            );
            shutdown(cx, &fixture);
        }
    }
}
