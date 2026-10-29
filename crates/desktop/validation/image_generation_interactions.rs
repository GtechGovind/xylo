//! Actual native Images controls and real local HTTP output, plus stale-response
//! regression checks. No account, clipboard device or paid image service is used.
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
    #[cfg(test)]
    mod generation_native_tests {
        use super::{Page, Xylo};
        use gpui_kit::{
            AppContext, Bounds, ClipboardItem, Image, ImageFormat, TestAppContext, WindowBounds,
            WindowOptions, px, size,
            test::{TestQueryExt, TestWindowExt},
        };
        use std::{
            io::{BufRead, Read, Write},
            time::{Duration, Instant},
        };
        use uuid::Uuid;
        use xylo_core::*;
        use xylo_runtime::{
            AppPaths,
            host::{HostClient, HostEvent},
        };
        use xylo_services::{Store, image_generation::ImageGenerationService};
        const PNG: &[u8] = b"\x89\x50\x4e\x47\x0d\x0a\x1a\x0a\x00\x00\x00\x0d\x49\x48\x44\x52\x00\x00\x00\x02\x00\x00\x00\x02\x08\x06\x00\x00\x00\x72\xb6\x0d\x24\x00\x00\x00\x11\x49\x44\x41\x54\x78\x9c\x63\x50\x70\x48\xf8\x0f\xc2\x0c\x30\x06\x00\x39\x74\x06\xfd\x20\x19\xfd\xfb\x00\x00\x00\x00\x49\x45\x4e\x44\xae\x42\x60\x82";
        fn wait(
            cx: &mut TestAppContext,
            view: &gpui_kit::Entity<Xylo>,
            description: &str,
            condition: impl Fn(&Xylo, &gpui_kit::App) -> bool,
        ) {
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                cx.run_until_parked();
                if cx.update(|cx| condition(view.read(cx), cx)) {
                    return;
                }
                assert!(Instant::now() < deadline, "Timed out: {description}");
                cx.executor().advance_clock(Duration::from_millis(2));
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        fn open(
            cx: &mut TestAppContext,
            paths: AppPaths,
        ) -> (
            HostClient,
            gpui_kit::AnyWindowHandle,
            gpui_kit::Entity<Xylo>,
        ) {
            cx.executor().allow_parking();
            cx.update(gpui_kit::init);
            let host = HostClient::spawn_at(paths).unwrap();
            let client = host.0.clone();
            let (window, view) = cx.update(|cx| {
                gpui_kit::open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds {
                            origin: Default::default(),
                            size: size(px(1440.), px(1800.)),
                        })),
                        ..Default::default()
                    },
                    cx,
                    |window, cx| cx.new(|cx| Xylo::new(Ok(host), window, cx)),
                )
                .unwrap()
            });
            wait(cx, &view, "settings", |view, _| view.settings_loaded);
            // Retired primary routes now migrate to Chat. Exercise the retained
            // image provider configuration explicitly rather than restoring it.
            cx.update_window(window, |_, _, cx| {
                view.update(cx, |view, cx| view.navigate(Page::Images, cx))
            })
            .unwrap();
            wait(cx, &view, "image provider configuration", |view, _| {
                !view.image_generation.validation_provider_pending()
            });
            (client, window, view)
        }
        fn provider(endpoint: String) -> ImageGenerationProvider {
            ImageGenerationProvider {
                name: "Native image fixture".into(),
                protocol: ImageGenerationProtocol::OpenAiImages,
                endpoint,
                model: "explicit-image-model".into(),
                credential_ref: None,
                headers: Default::default(),
                timeout_seconds: 10,
            }
        }
        fn reply(socket: &mut std::net::TcpStream, body: &[u8], content: &str) {
            write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: {content}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).unwrap();
            socket.write_all(body).unwrap();
        }
        fn http_request(
            socket: std::net::TcpStream,
        ) -> (std::net::TcpStream, String, serde_json::Value) {
            socket
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut socket = std::io::BufReader::new(socket);
            let mut line = String::new();
            socket.read_line(&mut line).unwrap();
            let first = line.clone();
            let mut length = 0;
            loop {
                line.clear();
                socket.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some((name, value)) = line.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    length = value.trim().parse::<usize>().unwrap();
                }
            }
            assert!(length <= 64 * 1024);
            let mut bytes = vec![0; length];
            socket.read_exact(&mut bytes).unwrap();
            let body = if bytes.is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::from_slice(&bytes).unwrap()
            };
            (socket.into_inner(), first, body)
        }
        #[gpui_kit::test]
        fn image_provider_setup_is_a_settings_form_and_saves_the_actual_connection(
            cx: &mut TestAppContext,
        ) {
            let temp = tempfile::tempdir().unwrap();
            let paths = AppPaths::at(temp.path().join("profile")).unwrap();
            let service =
                ImageGenerationService::new(paths.data.clone(), paths.artifacts.clone()).unwrap();
            let (client, window, view) = open(cx, paths);
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| view.navigate(Page::Providers, cx));
                window.render_frame(cx);
                window.click("configure-image-provider", cx);
                window.render_frame(cx);
                assert!(view.read(cx).settings_open);
                assert!(window.try_find("image-provider-settings").is_some());
                assert!(
                    window.try_find("generate-image").is_none(),
                    "Image generation happens in chat, not Settings"
                );
                assert!(window.try_find("refresh-image-history").is_none());
                view.update(cx, |view, cx| {
                    view.image_generation.name.update(cx, |input, cx| {
                        input.set_value("Fresh image connection", window, cx)
                    });
                    view.image_generation.endpoint.update(cx, |input, cx| {
                        input.set_value("http://127.0.0.1:18181/v1", window, cx)
                    });
                    view.image_generation.model.update(cx, |input, cx| {
                        input.set_value("explicit-image-model", window, cx)
                    });
                });
                window.render_frame(cx);
                window.click("image-api-gemini", cx);
                window.render_frame(cx);
                window.click("image-save-provider", cx);
            })
            .unwrap();
            wait(cx, &view, "saved actual image provider", |view, _| {
                view.image_generation
                    .notice
                    .starts_with("Saved image provider")
            });
            let saved = service.configured_provider().unwrap().unwrap();
            assert_eq!(saved.name, "Fresh image connection");
            assert_eq!(saved.model, "explicit-image-model");
            assert_eq!(saved.endpoint, "http://127.0.0.1:18181/v1");
            assert!(saved.credential_ref.is_none());
            assert_eq!(
                saved.protocol,
                ImageGenerationProtocol::GeminiGenerateContent
            );
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.image_generation.model.update(cx, |input, cx| {
                        input.set_value("unsaved-image-model", window, cx)
                    })
                });
                window.render_frame(cx);
                window.click("image-provider-back", cx);
                window.render_frame(cx);
                assert_eq!(view.read(cx).page, Page::Providers);
                assert_eq!(
                    service.configured_provider().unwrap().unwrap().model,
                    "explicit-image-model"
                );
                window.click("configure-image-provider", cx);
            })
            .unwrap();
            wait(cx, &view, "reopened saved image connection", |view, cx| {
                !view.image_generation.validation_provider_pending()
                    && view.image_generation.model.read(cx).value().as_ref()
                        == "explicit-image-model"
            });
            client.shutdown().unwrap();
            cx.run_until_parked();
        }
        #[gpui_kit::test]
        fn configured_image_provider_generates_actual_output_and_native_clipboard_pixels_attach(
            cx: &mut TestAppContext,
        ) {
            let temp = tempfile::tempdir().unwrap();
            let paths = AppPaths::at(temp.path().join("profile")).unwrap();
            let store = Store::open(&paths.database).unwrap();
            let mut settings = store.settings().unwrap();
            settings.ui.active_page = "images".into();
            store.save_settings(&settings).unwrap();
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let service =
                ImageGenerationService::new(paths.data.clone(), paths.artifacts.clone()).unwrap();
            service.save_provider(&provider(endpoint.clone())).unwrap();
            let server = std::thread::spawn(move || {
                let (socket, _) = listener.accept().unwrap();
                let (mut socket, line, body) = http_request(socket);
                assert!(line.starts_with("POST /v1/images/generations "));
                assert_eq!(body["model"], "explicit-image-model");
                assert_eq!(body["prompt"], "A native fixture image");
                reply(
                    &mut socket,
                    serde_json::json!({"data":[{"url":format!("{endpoint}/output.png")}]})
                        .to_string()
                        .as_bytes(),
                    "application/json",
                );
                drop(socket);
                let (socket, _) = listener.accept().unwrap();
                let (mut socket, line, _) = http_request(socket);
                assert!(line.starts_with("GET /output.png "));
                reply(&mut socket, PNG, "image/png");
            });
            let (client, window, view) = open(cx, paths.clone());
            wait(cx, &view, "saved image model", |view, cx| {
                view.image_generation.model.read(cx).value().as_ref() == "explicit-image-model"
            });
            let id = Uuid::new_v4();
            let session = cx.update(|cx| view.read(cx).session);
            cx.update_window(window, |_, _, cx| {
                view.update(cx, |view, _| {
                    view.image_generation.validation_prepare(id, session)
                })
            })
            .unwrap();
            // Generation belongs to chat/agent execution. Exercise the real host
            // and HTTP service here without restoring a Studio button in Settings.
            client
                .generate_image(
                    id,
                    ImageGenerationRequest {
                        id,
                        session_id: session,
                        provider: service.configured_provider().unwrap().unwrap(),
                        prompt: "A native fixture image".into(),
                        size: "1024x1024".into(),
                    },
                )
                .unwrap();
            wait(cx, &view, "generated local output", |view, _| {
                !view.image_generation.busy()
                    && view
                        .image_generation
                        .validation_receipt()
                        .is_some_and(|receipt| receipt.state == ImageGenerationState::Succeeded)
            });
            server.join().unwrap();
            let receipt = cx.update(|cx| {
                view.read(cx)
                    .image_generation
                    .validation_receipt()
                    .unwrap()
                    .clone()
            });
            assert_eq!(receipt.images.len(), 1);
            cx.update(|cx| {
                assert!(
                    view.read(cx)
                        .artifacts
                        .contains(&receipt.images[0].artifact_path)
                )
            });
            assert_eq!(
                std::fs::read(&receipt.images[0].artifact_path).unwrap(),
                PNG
            );
            assert_eq!(service.recent_receipts(16).unwrap()[0].id, receipt.id);
            // The chat card receives a managed reference from the tool result.
            // Only the real host may resolve it to a local preview path.
            let generated = receipt.images[0].attachment.clone();
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    view.page=Page::Chat;
                    view.messages.push(crate::state::message(view.session,"user","Generate an image".into()));
                    let action=Uuid::new_v4();
                    let turn=view.turns.entry(view.session).or_default();
                    turn.apply(EngineEvent::ToolStarted { action_id:action,name:"image_generate".into(),arguments:serde_json::json!({"prompt":"A native fixture image"}) });
                    turn.apply(EngineEvent::ToolFinished { action_id:action,name:"image_generate".into(),result:serde_json::json!({"summary":{"images":[generated.clone()]},"artifacts":["/untrusted/path-must-not-be-used.png"]}).to_string(),success:true });
                    view.sync_history_list(cx);
                });
                window.render_frame(cx);
                assert!(view.read(cx).attachments.validation_preview(generated.id).is_none());
                window.click(gpui_kit::SharedString::from(format!("image-preview-{}",generated.id)),cx);
            }).unwrap();
            wait(
                cx,
                &view,
                "host-validated generated chat preview",
                |view, _| view.attachments.validation_preview(generated.id).is_some(),
            );
            cx.update(|cx| {
                let preview = view
                    .read(cx)
                    .attachments
                    .validation_preview(generated.id)
                    .unwrap();
                assert!(
                    preview
                        .canonicalize()
                        .unwrap()
                        .starts_with(paths.artifacts.canonicalize().unwrap())
                );
                assert_eq!(image::image_dimensions(preview).unwrap(), (2, 2));
            });
            cx.update_window(window, |_, _, cx| {
                view.update(cx, |view, cx| view.navigate(Page::Images, cx))
            })
            .unwrap();

            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    assert!(view.add_draft_attachment(receipt.images[0].attachment.clone(), cx));
                    view.attachment_previews(&[receipt.images[0].attachment.clone()], cx);
                });
                assert_eq!(
                    view.read(cx).draft_attachments()[0].source,
                    AttachmentSource::Generated
                );
                view.update(cx, |view, cx| view.navigate(Page::Chat, cx));
                cx.write_to_clipboard(ClipboardItem::new_image(&Image::from_bytes(
                    ImageFormat::Png,
                    PNG.to_vec(),
                )));
                window.render_frame(cx);
                window.click("composer-context-actions", cx);
                window.within("popup-menu").click(1usize, cx);
            })
            .unwrap();
            wait(cx, &view, "native clipboard pixels", |view, _| {
                view.draft_attachments().len() == 2 && !view.attachments.importing()
            });
            cx.update(|cx| {
                let refs = view.read(cx).draft_attachments();
                assert_eq!(refs[1].source, AttachmentSource::Clipboard);
                assert!(serde_json::to_string(&refs).unwrap().len() < 4096);
            });
            client.shutdown().unwrap();
            cx.run_until_parked();
        }
        #[gpui_kit::test]
        fn stale_config_generation_and_cancelled_results_cannot_replace_current_form_or_draft(
            cx: &mut TestAppContext,
        ) {
            let temp = tempfile::tempdir().unwrap();
            let paths = AppPaths::at(temp.path().join("profile")).unwrap();
            let (client, window, view) = open(cx, paths);
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    let config_id = Uuid::new_v4();
                    view.image_generation.validation_config_read(config_id, cx);
                    view.image_generation.model.update(cx, |input, cx| {
                        input.set_value("user-edited-model", window, cx)
                    });
                    view.event(
                        HostEvent::ImageGenerationProvider {
                            request_id: config_id,
                            result: Ok(Some(provider("http://127.0.0.1:1".into()))),
                        },
                        window,
                        cx,
                    );
                    assert_eq!(
                        view.image_generation.model.read(cx).value().as_ref(),
                        "user-edited-model"
                    );
                    let id = Uuid::new_v4();
                    let session = view.session;
                    view.image_generation.validation_prepare(id, session);
                    let receipt = ImageGenerationReceipt {
                        id,
                        session_id: session,
                        state: ImageGenerationState::OutcomeUnknown,
                        provider_name: "Selected".into(),
                        model: "Selected model".into(),
                        created_at: now_ms(),
                        updated_at: now_ms(),
                        dispatch_started: true,
                        provider_request_id: None,
                        images: Vec::new(),
                        message: "Provider outcome unknown".into(),
                    };
                    view.event(
                        HostEvent::ImageGeneration {
                            request_id: Uuid::new_v4(),
                            result: Ok(receipt.clone()),
                        },
                        window,
                        cx,
                    );
                    assert!(view.image_generation.busy());
                    assert!(view.image_generation.validation_receipt().is_none());
                    view.new_chat(cx);
                    assert_ne!(view.session, session);
                    view.event(
                        HostEvent::ImageGeneration {
                            request_id: id,
                            result: Ok(receipt),
                        },
                        window,
                        cx,
                    );
                    assert!(!view.image_generation.busy());
                    assert!(view.image_generation.validation_needs_reconciliation());
                    assert!(view.draft_attachments().is_empty());
                    let error_id = Uuid::new_v4();
                    view.image_generation
                        .validation_prepare(error_id, view.session);
                    view.event(
                        HostEvent::ImageGeneration {
                            request_id: error_id,
                            result: Err("Invalid chosen endpoint".into()),
                        },
                        window,
                        cx,
                    );
                    assert!(!view.image_generation.busy());
                    assert!(view.image_generation.validation_needs_reconciliation());
                    assert!(view.draft_attachments().is_empty());
                })
            })
            .unwrap();
            client.shutdown().unwrap();
            cx.run_until_parked();
        }
    }
}
