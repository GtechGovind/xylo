//! Controlled offscreen captures of the production Xylo and composer.
//! A real temporary Host/Store loads a synthetic local configuration and empty
//! project. No prompt is sent, no account is read and no device is activated.
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

    #[cfg(test)]
    impl Xylo {
        pub(crate) fn composer_capture_ready(&self) -> bool {
            self.settings_loaded
                && self.workspace_ready
                && self
                    .drafts
                    .validation_revision(&xylo_runtime::drafts::DraftKey {
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

        pub(crate) fn composer_capture_history(&mut self, populated: bool, cx: &mut Context<Self>) {
            self.messages = if populated {
                (0..300)
                    .map(|index| {
                        message(
                            self.session,
                            if index % 2 == 0 { "user" } else { "assistant" },
                            format!(
                                "History message {index}: {}",
                                "Scrollable conversation text. ".repeat(10)
                            ),
                        )
                    })
                    .collect()
            } else {
                Vec::new()
            };
            self.sync_history_list(cx);
            cx.notify();
        }

        pub(crate) fn provider_capture(
            &mut self,
            custom: bool,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) {
            self.settings_open = true;
            self.page = Page::Providers;
            self.provider_form_open = true;
            self.choose_provider_kind(if custom { "Custom" } else { "OpenAI" }, window, cx);
            if custom {
                Self::set_input(&self.provider_name, "Custom provider", window, cx);
                Self::set_input(
                    &self.endpoint,
                    "https://provider.example.invalid/v1",
                    window,
                    cx,
                );
                Self::set_input(
                    &self.provider_discovery_url,
                    "https://provider.example.invalid/catalog",
                    window,
                    cx,
                );
                self.provider_headers.update(cx, |input, cx| {
                    input.set_value("{\"X-Project\":\"local-fixture\"}", window, cx)
                });
            }
            cx.notify();
        }

        pub(crate) fn account_capture(
            &mut self,
            ready: bool,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) {
            self.settings_open = true;
            self.page = Page::Providers;
            self.provider_form_open = true;
            let account = ProviderConfig {
                name: "ChatGPT".into(),
                family: ApiFamily::OpenAiResponses,
                endpoint: "https://api.openai.com/v1".into(),
                credential_ref: Some(CHATGPT_REF.into()),
                ..Default::default()
            };
            self.provider_form(&account, window, cx);
            // A connected, authorized account with no saved ChatGPT connection
            // loads its models as soon as its status arrives, so discovery is
            // captured before the status is delivered.
            self.provider_discovery.test_dispatch = true;
            self.account_result("account_chatgpt_status",&serde_json::json!({"connected":true,"plan_usage_authorized":true,"email":"fixture-account@example.invalid"}),window,cx);
            if ready {
                let (generation, provider) = self
                    .provider_discovery
                    .test_requests
                    .last()
                    .unwrap()
                    .clone();
                self.event(
                    HostEvent::Models {
                        generation,
                        provider,
                        models: vec!["gpt-fixture-model".into(), "gpt-fixture-reasoning".into()],
                        context_limits: Default::default(),
                        reasoning_options: Default::default(),
                        default_reasoning_efforts: Default::default(),
                    },
                    window,
                    cx,
                );
            }
        }

        pub(crate) fn queued_capture(&mut self, active: bool, cx: &mut Context<Self>) {
            self.settings_open = false;
            self.page = Page::Chat;
            self.settings.ui.work_panel_open = false;
            self.messages = vec![
                message(
                    self.session,
                    "user",
                    "Review the project and check the current changes.".into(),
                ),
                message(
                    self.session,
                    "assistant",
                    "I’m reviewing the changed files and checking the affected tests.".into(),
                ),
            ];
            if active {
                self.turns
                    .entry(self.session)
                    .or_default()
                    .apply(EngineEvent::Started);
            } else {
                self.turns.remove(&self.session);
            }
            let items = [
                "Then run the affected tests and report any failures.",
                "Summarize the changes and anything that still needs attention.",
            ]
            .into_iter()
            .map(|prompt| {
                let turn = TurnRequest {
                    approved_rules: Vec::new(),
                    session_id: self.session,
                    turn_id: Uuid::new_v4(),
                    workspace: self.workspace.clone().unwrap(),
                    prompt: prompt.into(),
                    provider: self.settings.provider.clone(),
                    permissions: self.settings.permissions,
                    attachments: vec![],
                };
                xylo_runtime::turn_queue::QueuedTurn {
                    id: turn.turn_id,
                    turn,
                    paused: !active,
                    reason: None,
                }
            })
            .collect();
            self.turn_queue.items.insert(self.session, items);
            cx.notify();
        }
        pub(crate) fn reset_queued_capture(&mut self, cx: &mut Context<Self>) {
            self.turns.remove(&self.session);
            self.turn_queue.items.remove(&self.session);
            self.messages.clear();
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
    use xylo_core::{AppSettings, PermissionMode};
    use xylo_runtime::{AppPaths, host::HostClient};
    use xylo_services::Store;

    let temporary = tempfile::tempdir().unwrap();
    let workspace = temporary.path().join("Empty project");
    std::fs::create_dir(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();
    let paths = AppPaths::at(temporary.path().join("profile")).unwrap();
    let store = Store::open(&paths.database).unwrap();
    let mut settings = AppSettings {
        last_workspace: Some(workspace.clone()),
        permissions: PermissionMode::AskForApproval,
        theme: "light".into(),
        ..Default::default()
    };
    // This is a persisted display configuration, not an available/live model.
    settings.provider.model = "fixture-local-coder-model".into();
    settings.provider.reasoning_options = Some(vec![
        "low".into(),
        "medium".into(),
        "high".into(),
        "xhigh".into(),
    ]);
    settings.provider.reasoning_effort = Some("high".into());
    settings.remember_model(settings.provider.clone());
    store.save_settings(&settings).unwrap();
    let host = HostClient::spawn_at(paths).unwrap();
    let client = host.0.clone();
    let mut cx = HeadlessAppContext::with_platform(
        platform::current_platform(true).text_system(),
        Arc::new(crate::assets::AppAssets),
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
                |window, cx| cx.new(|cx| xylo::Xylo::new(Ok(host), window, cx)),
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

    let output = capture_dir::fresh("composer-rendering");
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
                    // The context ring stays hidden until a chat uses 5% of the window.
                    let ids = [
                        "composer-context-actions",
                        "composer-permissions",
                        "composer-model",
                        "record-voice-composer",
                        "send-prompt",
                    ];
                    assert_eq!(window.find("composer-context-actions").label(), Some("Add context"));
                    assert_eq!(window.find("composer-permissions").label(), Some("Ask for approval"));
                    assert!(window.try_find("composer-context").is_none(), "A fresh chat has no context ring");
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
                    assert!(window.try_find("composer-project").is_none(), "Project management belongs in the sidebar");
                    assert!(window.try_find("permission-mode").is_none(), "The toolbar must contain one permission selector");
                    assert!(window.try_find(("assistant", 0usize)).is_none(), "The empty chat must not contain showcase cards");
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
            cx.update_window(handle, |_, window, cx| {
                window.click("composer-model", cx);
                let panel = window.find("composer-model-panel").bounds();
                let effort = window.find("composer-effort").bounds();
                assert!(
                    panel.size.width <= px(340.) && effort.bottom() <= panel.bottom(),
                    "Model and effort controls must stay compact inside the picker"
                );
            })
            .unwrap();
            let popup_image = format!("composer-{theme}-{width}-model-popover.png");
            cx.capture_screenshot(handle)
                .unwrap()
                .save(output.join(&popup_image))
                .unwrap();
            cx.update_window(handle, |_, window, cx| window.click("composer-model", cx))
                .unwrap();
            // A long, virtualized conversation scrolls above the composer;
            // the existing-chat composer must stay at the viewport bottom.
            let anchor = cx.update_window(handle, |_, window, cx| {
                view.update(cx, |view, cx| view.composer_capture_history(true, cx));
                window.render_frame(cx);
                let send = window.find("send-prompt").bounds();
                let input = window.find_by_label("Message").bounds();
                assert!(send.bottom() <= px(880.) && send.bottom() >= px(820.), "Existing-chat composer must stay at the bottom: {send:?}");
                assert!(input.top() > px(680.), "History must not displace the composer: {input:?}");
                assert!(window.find_all_by_label("Copy").len() < 50, "Long history must remain virtualized");
                let anchor = serde_json::json!({"send_bottom":f32::from(send.bottom()),"input_top":f32::from(input.top()),"history_messages":300});
                view.update(cx, |view, cx| view.composer_capture_history(false, cx));
                window.render_frame(cx);
                anchor
            }).unwrap();
            cases.push(serde_json::json!({
                "geometry":geometry,"existing_chat_anchor":anchor,"image":filename,"model_popover_image":popup_image,
                "pixels":[screenshot.width(),screenshot.height()],
                "sha256":hex::encode(Sha256::digest(std::fs::read(destination).unwrap())),
            }));
        }
    }
    let mut queue_cases = Vec::new();
    for theme in ["light", "dark"] {
        settings.theme = theme.into();
        client.save_settings(settings.clone()).unwrap();
        wait(&mut cx, theme);
        for (width, height) in [(1000_u32, 680_u32), (1380, 880), (1720, 1040)] {
            for active in [true, false] {
                cx.update_window(handle, |_, window, cx| {
                    window.resize(size(px(width as f32), px(height as f32)));
                    window.bounds_changed(cx);
                    view.update(cx, |view, cx| view.queued_capture(active, cx));
                    window.render_frame(cx);
                    let pending = window.find("composer-pending").bounds();
                    let composer = window.find("image-drop-target").bounds();
                    assert!(pending.bottom() <= composer.top());
                    assert!(
                        composer.bottom() >= px(height as f32 - 35.)
                            && composer.bottom() <= px(height as f32)
                    );
                    assert_eq!(window.try_find("steer-prompt").is_some(), active);
                    assert_eq!(window.try_find(("send-queued", 0usize)).is_some(), !active);
                    assert!(window.find(("remove-queued", 0usize)).visible());
                })
                .unwrap();
                let filename = format!(
                    "queue-{theme}-{width}-{}.png",
                    if active { "active" } else { "paused" }
                );
                cx.capture_screenshot(handle)
                    .unwrap()
                    .save(output.join(&filename))
                    .unwrap();
                queue_cases.push(serde_json::json!({"theme":theme,"viewport":[width,height],"active":active,"image":filename}));
                cx.update_window(handle, |_, _, cx| {
                    view.update(cx, |view, cx| view.reset_queued_capture(cx))
                })
                .unwrap();
            }
        }
    }
    let mut provider_cases = Vec::new();
    for theme in ["light", "dark"] {
        settings.theme = theme.into();
        client.save_settings(settings.clone()).unwrap();
        wait(&mut cx, theme);
        for custom in [true, false] {
            cx.update_window(handle, |_, window, cx| {
                window.resize(size(px(1380.), px(1100.)));
                window.bounds_changed(cx);
                view.update(cx, |view, cx| view.provider_capture(custom, window, cx));
                window.render_frame(cx);
                assert!(window.try_find("provider-save").is_some());
                assert_eq!(
                    window
                        .try_find_by_label("Extra headers (JSON object)")
                        .is_some(),
                    custom
                );
                assert_eq!(
                    window
                        .try_find_by_label("Model discovery URL (optional)")
                        .is_some(),
                    custom
                );
                assert_eq!(window.try_find_by_label("Base URL").is_some(), custom);
                assert!(window.try_find("preset-azure").is_none());
                assert!(window.try_find("preset-bedrock").is_none());
                assert!(window.try_find("provider-discover").is_none());
                assert!(
                    window.try_find("preset-local").is_none(),
                    "Selected provider forms must not keep a provider showcase"
                );
            })
            .unwrap();
            cx.run_until_parked();
            let screenshot = cx.capture_screenshot(handle).unwrap();
            let filename = format!(
                "provider-{theme}-{}.png",
                if custom { "custom" } else { "openai" }
            );
            screenshot.save(output.join(&filename)).unwrap();
            provider_cases.push(serde_json::json!({"theme":theme,"provider":if custom {"custom"} else {"openai"},"image":filename,"viewport":[1380,1100],"no_request_submitted":true}));
        }
    }
    let mut account_cases = Vec::new();
    for theme in ["light", "dark"] {
        settings.theme = theme.into();
        client.save_settings(settings.clone()).unwrap();
        wait(&mut cx, theme);
        for ready in [false, true] {
            cx.update_window(handle, |_, window, cx| {
                view.update(cx, |view, cx| view.account_capture(ready, window, cx));
                window.render_frame(cx);
                assert!(window.try_find("chatgpt-refresh-models").is_some());
                assert_eq!(
                    window.try_find("provider-cancel-discovery").is_some(),
                    !ready
                );
                assert_eq!(window.try_find(("model", 0usize)).is_some(), ready);
            })
            .unwrap();
            let screenshot = cx.capture_screenshot(handle).unwrap();
            let filename = format!(
                "account-{theme}-{}.png",
                if ready { "models" } else { "loading" }
            );
            screenshot.save(output.join(&filename)).unwrap();
            account_cases.push(serde_json::json!({"theme":theme,"state":if ready {"models"} else {"loading"},"image":filename,"controlled_account_status_and_catalog":true,"live_account_qualified":false,"no_auth_or_catalog_request_submitted":true}));
        }
    }
    let compiled_sources = [
        (
            "accounts.rs",
            include_bytes!("../src/xylo/accounts.rs").as_slice(),
        ),
        ("xylo.rs", include_bytes!("../src/xylo.rs").as_slice()),
        ("chat.rs", include_bytes!("../src/xylo/chat.rs").as_slice()),
        (
            "providers.rs",
            include_bytes!("../src/xylo/providers.rs").as_slice(),
        ),
        (
            "settings.rs",
            include_bytes!("../src/xylo/settings.rs").as_slice(),
        ),
        (
            "attachments.rs",
            include_bytes!("../src/xylo/attachments.rs").as_slice(),
        ),
        (
            "speech.rs",
            include_bytes!("../src/xylo/speech.rs").as_slice(),
        ),
        ("ui.rs", include_bytes!("../src/ui.rs").as_slice()),
        (
            "composer_rendering.rs",
            include_bytes!("composer_rendering.rs").as_slice(),
        ),
        ("native-Cargo.lock", include_bytes!("Cargo.lock").as_slice()),
    ]
    .into_iter()
    .map(|(path, bytes)| (path, hex::encode(Sha256::digest(bytes))))
    .collect::<std::collections::BTreeMap<_, _>>();
    let metadata = serde_json::json!({
        "schema":1,
        "scope":"Controlled offscreen production Xylo capture with real temporary Host/Store, native platform fonts and GPU. Synthetic persisted model label, no inference/accounts/microphone or physical desktop acceptance.",
        "queue_cases":queue_cases,"cases":cases,"provider_cases":provider_cases,"account_cases":account_cases,"compiled_sources":compiled_sources,
        "source_errors_suppressed":false,
        "controls_clicked":true,"execution_actions_submitted":false,
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
