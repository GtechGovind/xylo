//! Native account controls with controlled authorization/status/catalog events.
//! Never signs into a real account, reads live credentials, or sends inference.
#![allow(dead_code, unused_imports)]
extern crate gpui_kit as gpui;
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
    mod account_flow_tests {
        use super::{ApiFamily, CHATGPT_REF, Page, ProviderConfig, Xylo};
        use gpui_kit::test::{TestQueryExt, TestWindowExt};
        use gpui_kit::{
            AppContext, Bounds, Entity, TestAppContext, WindowBounds, WindowOptions, px, size,
        };
        fn open(cx: &mut TestAppContext) -> (gpui_kit::AnyWindowHandle, Entity<Xylo>) {
            cx.update(gpui_kit::init);
            let (window, view) = cx.update(|cx| {
                gpui_kit::open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                            Default::default(),
                            size(px(1380.), px(1000.)),
                        ))),
                        ..Default::default()
                    },
                    cx,
                    |window, cx| {
                        cx.new(|cx| Xylo::new(Err(anyhow::anyhow!("No external host")), window, cx))
                    },
                )
                .unwrap()
            });
            (window, view)
        }
        #[gpui_kit::test]
        fn use_account_does_not_replace_an_active_model_before_selection(cx: &mut TestAppContext) {
            let (window, view) = open(cx);
            cx.update_window(window,|_,window,cx| {
                view.update(cx,|view,cx| {
                    view.error=None;
                    view.settings.provider.model="existing-model".into();
                    view.settings_open=true;view.page=Page::Providers;view.provider_form_open=true;
                    let account=ProviderConfig {name:"ChatGPT".into(),family:ApiFamily::OpenAiResponses,endpoint:"https://api.openai.com/v1".into(),credential_ref:Some(CHATGPT_REF.into()),..Default::default()};
                    view.provider_form(&account,window,cx);
                    view.account_result("account_chatgpt_status",&serde_json::json!({"connected":true,"plan_usage_authorized":true,"email":"fixture@example.invalid"}),window,cx);
                });
                window.render_frame(cx);
                window.click("chatgpt-refresh-models",cx);
                assert_eq!(view.read(cx).settings.provider.model,"existing-model","Use account must stage discovery, not erase the working model");
            }).unwrap();
        }

        fn prepare(
            view: &mut Xylo,
            connected: bool,
            authorized: bool,
            window: &mut gpui_kit::Window,
            cx: &mut gpui_kit::Context<Xylo>,
        ) {
            view.error = None;
            view.settings_open = true;
            view.page = Page::Providers;
            view.provider_form_open = true;
            let account = ProviderConfig {
                name: "ChatGPT".into(),
                family: ApiFamily::OpenAiResponses,
                endpoint: "https://api.openai.com/v1".into(),
                credential_ref: Some(CHATGPT_REF.into()),
                ..Default::default()
            };
            view.provider_form(&account, window, cx);
            view.provider_discovery.test_dispatch = true;
            view.account_result("account_chatgpt_status",&serde_json::json!({"connected":connected,"plan_usage_authorized":authorized,"email":"fixture@example.invalid"}),window,cx);
        }
        fn catalog(
            view: &mut Xylo,
            generation: u64,
            provider: ProviderConfig,
            models: Vec<String>,
            window: &mut gpui_kit::Window,
            cx: &mut gpui_kit::Context<Xylo>,
        ) {
            let reasoning = std::collections::BTreeMap::from([
                (
                    "gpt-fixture-authorized".into(),
                    vec!["low".into(), "high".into()],
                ),
                ("gpt-fixture-second".into(), Vec::new()),
            ]);
            view.event(
                xylo_runtime::host::HostEvent::Models {
                    generation,
                    provider,
                    models,
                    context_limits: Default::default(),
                    reasoning_options: reasoning,
                    default_reasoning_efforts: Default::default(),
                },
                window,
                cx,
            );
        }
        #[gpui_kit::test]
        fn account_identity_without_plan_consent_cannot_load_models(cx: &mut TestAppContext) {
            let (window, view) = open(cx);
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| prepare(view, true, false, window, cx));
                window.render_frame(cx);
                assert!(window.try_find("chatgpt-authorize-plan").is_some());
                assert!(window.try_find("chatgpt-refresh-models").is_none());
                view.update(cx, |view, cx| view.use_chatgpt(window, cx));
                assert!(view.read(cx).provider_discovery.test_requests.is_empty());
                view.update(cx, |view, cx| prepare(view, false, false, window, cx));
                window.render_frame(cx);
                assert!(window.try_find("chatgpt-sign-in").is_some());
                assert!(window.try_find("chatgpt-refresh-models").is_none());
            })
            .unwrap();
        }
        #[gpui_kit::test]
        fn account_loading_cancel_stale_error_empty_and_retry_are_visible(cx: &mut TestAppContext) {
            let (window, view) = open(cx);
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    prepare(view, true, true, window, cx);
                    view.settings.provider.model = "keep-existing".into();
                });
                window.render_frame(cx);
                window.click("chatgpt-refresh-models", cx);
                let (old, provider) = view
                    .read(cx)
                    .provider_discovery
                    .test_requests
                    .last()
                    .unwrap()
                    .clone();
                assert_eq!(provider.endpoint, "https://api.openai.com/v1");
                assert_eq!(provider.family, ApiFamily::OpenAiResponses);
                assert_eq!(provider.credential_ref.as_deref(), Some(CHATGPT_REF));
                assert!(view.read(cx).provider_discovery.loading());
                assert!(window.try_find("provider-cancel-discovery").is_some());
                window.click("provider-cancel-discovery", cx);
                view.update(cx, |view, cx| {
                    catalog(
                        view,
                        old,
                        provider.clone(),
                        vec!["stale".into()],
                        window,
                        cx,
                    )
                });
                assert!(view.read(cx).models.is_empty());
                window.render_frame(cx);
                window.click("provider-retry", cx);
                let (current, provider) = view
                    .read(cx)
                    .provider_discovery
                    .test_requests
                    .last()
                    .unwrap()
                    .clone();
                assert!(current > old);
                view.update(cx, |view, cx| {
                    catalog(
                        view,
                        old,
                        provider.clone(),
                        vec!["stale".into()],
                        window,
                        cx,
                    )
                });
                assert!(view.read(cx).models.is_empty());
                view.update(cx, |view, cx| {
                    view.event(
                        xylo_runtime::host::HostEvent::ModelsFailed {
                            generation: current,
                            provider: provider.clone(),
                            message: "Reconnect ChatGPT: expired authorization".into(),
                        },
                        window,
                        cx,
                    )
                });
                window.render_frame(cx);
                assert!(window.try_find("chatgpt-reconnect").is_some());
                assert!(window.try_find("provider-retry").is_some());
                assert_eq!(view.read(cx).settings.provider.model, "keep-existing");
                window.click("provider-retry", cx);
                let (generation, provider) = view
                    .read(cx)
                    .provider_discovery
                    .test_requests
                    .last()
                    .unwrap()
                    .clone();
                view.update(cx, |view, cx| {
                    catalog(view, generation, provider, vec![], window, cx)
                });
                window.render_frame(cx);
                assert!(window.try_find("provider-retry").is_some());
                assert!(window.try_find("available-models").is_none());
                assert!(!view.read(cx).provider_discovery.loading());
            })
            .unwrap();
        }
        #[gpui_kit::test]
        fn cancelled_auth_result_cannot_start_discovery_or_reconnect_the_view(
            cx: &mut TestAppContext,
        ) {
            let (window, view) = open(cx);
            let request = uuid::Uuid::new_v4();
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    prepare(view, false, false, window, cx);
                    view.account
                        .validation_pending(request, "account_chatgpt_sign_in");
                });
                window.render_frame(cx);
                window.click("chatgpt-cancel-sign-in", cx);
                view.update(cx, |view, cx| {
                    assert!(view.account_capability_result(
                        request,
                        "account_chatgpt_sign_in",
                        &Ok(serde_json::json!({"connected":true,"plan_usage_authorized":true})),
                        window,
                        cx
                    ));
                });
                window.render_frame(cx);
                assert!(window.try_find("chatgpt-sign-in").is_some());
                assert!(window.try_find("chatgpt-refresh-models").is_none());
                assert!(view.read(cx).provider_discovery.test_requests.is_empty());
            })
            .unwrap();
        }
        #[gpui_kit::test]
        fn future_model_catalog_automatically_registers_reasoning_and_honors_explicit_unsupported(
            cx: &mut TestAppContext,
        ) {
            let (window, view) = open(cx);
            cx.update_window(window,|_,window,cx| {
                view.update(cx,|view,cx|prepare(view,true,true,window,cx));window.render_frame(cx);window.click("chatgpt-refresh-models",cx);
                let (generation,provider)=view.read(cx).provider_discovery.test_requests.last().unwrap().clone();
                view.update(cx,|view,cx|view.event(xylo_runtime::host::HostEvent::Models {
                    generation,provider,models:vec!["future-model-v99".into(),"future-other-v99".into()],context_limits:Default::default(),
                    reasoning_options:std::collections::BTreeMap::from([("future-model-v99".into(),vec!["low".into(),"high".into(),"xhigh".into()]),("future-other-v99".into(),Vec::new())]),
                    default_reasoning_efforts:std::collections::BTreeMap::from([("future-model-v99".into(),"high".into())]),
                },window,cx));
                assert_eq!(view.read(cx).settings.provider.model,"future-model-v99","With no usable model active, the first discovered model is activated");
                assert!(view.read(cx).settings.providers.iter().any(|provider|provider.model=="future-other-v99"));
                assert_eq!(view.read(cx).settings.provider.default_reasoning_effort.as_deref(),Some("high"));
                view.update(cx,|view,cx|{view.settings_open=false;view.page=Page::Chat;cx.notify();});window.render_frame(cx);
                window.click("composer-model",cx);assert!(window.try_find("composer-effort").is_some(),"A model with reasoning options offers the effort menu");window.click("composer-model",cx);
                view.update(cx,|view,_|{view.settings.provider.reasoning_effort=Some("high".into());let provider=view.settings.provider.clone();view.settings.remember_model(provider);});
                assert_eq!(view.read(cx).settings.provider.reasoning_effort.as_deref(),Some("high"));
                view.update(cx,|view,cx| {view.settings_open=true;view.page=Page::Providers;view.provider_form_open=true;view.use_chatgpt(window,cx);});
                let (generation,provider)=view.read(cx).provider_discovery.test_requests.last().unwrap().clone();
                view.update(cx,|view,cx|view.event(xylo_runtime::host::HostEvent::Models {
                    generation,provider,models:vec!["future-other-v99".into(),"future-model-v99".into()],context_limits:Default::default(),
                    reasoning_options:std::collections::BTreeMap::from([("future-model-v99".into(),vec!["low".into(),"high".into(),"xhigh".into()])]),default_reasoning_efforts:Default::default(),
                },window,cx));
                assert_eq!(view.read(cx).settings.provider.model,"future-model-v99","Refresh preserves the active model while it is supported, even when server order changes");
                assert_eq!(view.read(cx).settings.provider.reasoning_effort.as_deref(),Some("high"));
                view.update(cx,|view,cx|view.use_chatgpt(window,cx));let (generation,provider)=view.read(cx).provider_discovery.test_requests.last().unwrap().clone();
                view.update(cx,|view,cx|view.event(xylo_runtime::host::HostEvent::Models {generation,provider,models:vec!["gpt-5.2".into()],context_limits:Default::default(),reasoning_options:std::collections::BTreeMap::from([("gpt-5.2".into(),Vec::new())]),default_reasoning_efforts:Default::default()},window,cx));
                view.update(cx,|view,cx|{view.settings_open=false;view.page=Page::Chat;cx.notify();});window.render_frame(cx);
                assert!(view.read(cx).settings.provider.supported_reasoning_efforts().is_empty(),"Explicit catalog metadata overrides a known-name fallback");
                window.click("composer-model",cx);
                assert!(window.try_find("composer-effort").is_none(),"An explicitly unsupported model must not offer effort choices");
            }).unwrap();
        }
        #[gpui_kit::test]
        fn custom_provider_slow_success_failure_and_back_cancel_keep_feedback_scoped(
            cx: &mut TestAppContext,
        ) {
            let (window, view) = open(cx);
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.error = None;
                    view.settings_open = true;
                    view.page = Page::Providers;
                    view.provider_form_open = true;
                    view.choose_provider_kind("Custom", window, cx);
                    Xylo::set_input(
                        &view.endpoint,
                        "https://fixture.example.invalid/v1",
                        window,
                        cx,
                    );
                    view.provider_discovery.test_dispatch = true;
                });
                window.render_frame(cx);
                window.click("provider-save", cx);
                assert!(view.read(cx).provider_discovery.loading());
                assert!(window.try_find("provider-cancel-discovery").is_some());
                let (generation, provider) = view
                    .read(cx)
                    .provider_discovery
                    .test_requests
                    .last()
                    .unwrap()
                    .clone();
                view.update(cx, |view, cx| {
                    catalog(
                        view,
                        generation,
                        provider.clone(),
                        vec!["fixture-model".into()],
                        window,
                        cx,
                    )
                });
                window.render_frame(cx);
                assert!(window.try_find(("model", 0usize)).is_some());
                assert!(!view.read(cx).provider_discovery.loading());
                window.click("provider-save", cx);
                let (next, provider) = view
                    .read(cx)
                    .provider_discovery
                    .test_requests
                    .last()
                    .unwrap()
                    .clone();
                view.update(cx, |view, cx| {
                    view.event(
                        xylo_runtime::host::HostEvent::ModelsFailed {
                            generation: next,
                            provider: provider.clone(),
                            message: "Temporary fixture network failure".into(),
                        },
                        window,
                        cx,
                    )
                });
                window.render_frame(cx);
                assert!(window.try_find("provider-retry").is_some());
                window.click("provider-retry", cx);
                let (pending, provider) = view
                    .read(cx)
                    .provider_discovery
                    .test_requests
                    .last()
                    .unwrap()
                    .clone();
                window.click("provider-back", cx);
                view.update(cx, |view, cx| {
                    catalog(
                        view,
                        pending,
                        provider,
                        vec!["must-not-return".into()],
                        window,
                        cx,
                    )
                });
                assert!(view.read(cx).models.is_empty());
                assert!(!view.read(cx).provider_form_open);
            })
            .unwrap();
        }
        #[gpui_kit::test]
        fn failed_provider_save_keeps_the_key_and_never_stores_its_reference(
            cx: &mut TestAppContext,
        ) {
            let (window, view) = open(cx);
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.settings_open = true;
                    view.page = Page::Providers;
                    view.provider_form_open = true;
                    view.choose_provider_kind("OpenAI", window, cx);
                    Xylo::set_input(&view.secret, "sk-fixture", window, cx);
                    view.provider_discovery.test_dispatch = true;
                });
                window.render_frame(cx);
                window.click("provider-save", cx);
                let (generation, provider) = view
                    .read(cx)
                    .provider_discovery
                    .test_requests
                    .last()
                    .unwrap()
                    .clone();
                let reference = provider.credential_ref.clone();
                assert!(
                    reference.is_some(),
                    "A typed key is sent under a new reference"
                );
                view.update(cx, |view, cx| {
                    view.event(
                        xylo_runtime::host::HostEvent::ModelsFailed {
                            generation,
                            provider,
                            message: "Could not store the credential".into(),
                        },
                        window,
                        cx,
                    )
                });
                window.render_frame(cx);
                let state = view.read(cx);
                assert_eq!(state.secret.read(cx).value().as_ref(), "sk-fixture");
                assert!(
                    state
                        .provider_editor_source
                        .as_ref()
                        .is_some_and(|source| source.credential_ref.is_none()),
                    "The form keeps asking for the key"
                );
                assert!(
                    state
                        .settings
                        .providers
                        .iter()
                        .all(|saved| saved.credential_ref != reference),
                    "No stored provider points at a secret that was never stored"
                );
                assert!(window.try_find("provider-retry").is_some());
            })
            .unwrap();
        }
        #[gpui_kit::test]
        fn account_model_choice_updates_composer_and_real_temporary_store(cx: &mut TestAppContext) {
            use std::time::{Duration, Instant};
            let profile = tempfile::tempdir().unwrap();
            let paths = xylo_runtime::AppPaths::at(profile.path().join("profile")).unwrap();
            let store = xylo_services::Store::open(&paths.database).unwrap();
            cx.executor().allow_parking();
            cx.update(gpui_kit::init);
            let host = xylo_runtime::host::HostClient::spawn_at(paths).unwrap();
            let client = host.0.clone();
            let (window, view) = cx.update(|cx| {
                gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                    cx.new(|cx| Xylo::new(Ok(host), window, cx))
                })
                .unwrap()
            });
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                cx.executor().advance_clock(Duration::from_millis(2));
                cx.run_until_parked();
                if cx.update(|cx| view.read(cx).settings_loaded) {
                    break;
                }
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            }
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| prepare(view, true, true, window, cx));
                window.render_frame(cx);
                window.click("chatgpt-refresh-models", cx);
                let (generation, provider) = view
                    .read(cx)
                    .provider_discovery
                    .test_requests
                    .last()
                    .unwrap()
                    .clone();
                assert_eq!(generation, client.provider_discovery_generation());
                assert!(provider.headers.is_empty());
                assert!(provider.discovery_url.is_none());
                view.update(cx, |view, cx| {
                    catalog(
                        view,
                        generation,
                        provider,
                        vec!["gpt-fixture-authorized".into(), "gpt-fixture-second".into()],
                        window,
                        cx,
                    )
                });
                window.render_frame(cx);
                assert_eq!(
                    view.read(cx).settings.provider.model,
                    "gpt-fixture-authorized",
                    "A successful catalog must activate the connection without another click"
                );
                assert!(
                    view.read(cx)
                        .settings
                        .providers
                        .iter()
                        .any(|provider| provider.model == "gpt-fixture-second"),
                    "Every supported catalog model must be registered automatically"
                );
                assert!(
                    window.try_find(("model", 1usize)).is_some(),
                    "The provider page lists every available model read-only"
                );
                assert_eq!(
                    view.read(cx)
                        .settings
                        .providers
                        .iter()
                        .find(|provider| provider.model == "gpt-fixture-second")
                        .and_then(|provider| provider.reasoning_options.clone()),
                    Some(Vec::new()),
                    "Each model keeps its own catalog metadata"
                );
                view.update(cx, |view, cx| {
                    view.settings_open = false;
                    view.page = Page::Chat;
                    cx.notify();
                });
                window.render_frame(cx);
                // Models are chosen in the composer's model picker.
                let second = view
                    .read(cx)
                    .settings
                    .providers
                    .iter()
                    .position(|provider| provider.model == "gpt-fixture-second")
                    .unwrap();
                window.click("composer-model", cx);
                window.click(("composer-model-choice", second), cx);
                window.render_frame(cx);
                assert_eq!(
                    window.find("composer-model").label(),
                    Some("gpt-fixture-second")
                );
            })
            .unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                cx.executor().advance_clock(Duration::from_millis(2));
                cx.run_until_parked();
                if store.settings().unwrap().provider.model == "gpt-fixture-second" {
                    break;
                }
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            }
            cx.update_window(window,|_,window,cx|view.update(cx,|view,cx| {
                let mut second=view.settings.provider.clone();second.model="other-account-model".into();view.settings.providers.push(second);
                let stale=view.settings.clone();
                view.account_result("account_chatgpt_sign_out",&serde_json::json!({}),window,cx);
                assert!(view.settings.providers.iter().all(|provider|provider.credential_ref.as_deref()!=Some(CHATGPT_REF)));
                assert_ne!(view.settings.provider.credential_ref.as_deref(),Some(CHATGPT_REF));
                view.event(xylo_runtime::host::HostEvent::Settings(stale),window,cx);
                assert!(view.settings.providers.iter().all(|provider|provider.credential_ref.as_deref()!=Some(CHATGPT_REF)));
                assert_ne!(view.settings.provider.credential_ref.as_deref(),Some(CHATGPT_REF));
            })).unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                cx.executor().advance_clock(Duration::from_millis(2));
                cx.run_until_parked();
                let current = store.settings().unwrap();
                if current.provider.credential_ref.as_deref() != Some(CHATGPT_REF)
                    && current
                        .providers
                        .iter()
                        .all(|provider| provider.credential_ref.as_deref() != Some(CHATGPT_REF))
                {
                    break;
                }
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            }
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| prepare(view, true, true, window, cx));
                window.render_frame(cx);
                window.click("chatgpt-refresh-models", cx);
                let (generation, provider) = view
                    .read(cx)
                    .provider_discovery
                    .test_requests
                    .last()
                    .unwrap()
                    .clone();
                view.update(cx, |view, cx| {
                    catalog(
                        view,
                        generation,
                        provider,
                        vec!["gpt-fixture-authorized".into(), "gpt-fixture-second".into()],
                        window,
                        cx,
                    )
                });
                window.render_frame(cx);
                assert_eq!(
                    view.read(cx).settings.provider.reasoning_options,
                    Some(vec!["low".into(), "high".into()]),
                    "A refresh keeps the active model and its advertised options"
                );
                assert_eq!(
                    view.read(cx).settings.provider.model,
                    "gpt-fixture-authorized"
                );
            })
            .unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                cx.executor().advance_clock(Duration::from_millis(2));
                cx.run_until_parked();
                if store.settings().unwrap().provider.model == "gpt-fixture-authorized" {
                    break;
                }
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            }
            client.shutdown().unwrap();
            cx.executor().advance_clock(Duration::from_millis(2));
            cx.run_until_parked();
        }
    }
}
