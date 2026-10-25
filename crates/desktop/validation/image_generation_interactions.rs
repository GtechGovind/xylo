//! Actual native Images controls and real local HTTP output, plus stale-response
//! regression checks. No account, clipboard device or paid image service is used.
#![allow(dead_code, unused_imports)]
extern crate gpui_kit as gpui;
#[path = "../src/state.rs"]
mod state;
#[path = "../src/terminal_view.rs"]
mod terminal_view;
#[path = "../src/ui.rs"]
mod ui;
mod workbench {
    include!("../src/workbench.rs");
    #[cfg(test)]
    mod generation_native_tests {
        use super::{Page, Workbench};
        use gpui_kit::{
            AppContext, Bounds, ClipboardItem, Image, ImageFormat, TestAppContext, WindowBounds,
            WindowOptions, px, size, test::TestWindowExt,
        };
        use std::{
            io::{BufRead, Read, Write},
            time::{Duration, Instant},
        };
        use uuid::Uuid;
        use workbench_core::*;
        use workbench_runtime::{
            AppPaths,
            host::{HostClient, HostEvent},
        };
        use workbench_services::{Store, image_generation::ImageGenerationService};
        const PNG: &[u8] = include_bytes!("../../../packaging/icons/app.png");
        fn wait(
            cx: &mut TestAppContext,
            view: &gpui_kit::Entity<Workbench>,
            description: &str,
            condition: impl Fn(&Workbench, &gpui_kit::App) -> bool,
        ) {
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                cx.run_until_parked();
                if cx.update(|cx| condition(view.read(cx), cx)) {
                    return;
                }
                assert!(Instant::now() < deadline, "Timed out: {description}");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        fn open(
            cx: &mut TestAppContext,
            paths: AppPaths,
        ) -> (
            HostClient,
            gpui_kit::AnyWindowHandle,
            gpui_kit::Entity<Workbench>,
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
                    |window, cx| cx.new(|cx| Workbench::new(Ok(host), window, cx)),
                )
                .unwrap()
            });
            wait(cx, &view, "settings", |view, _| view.settings_loaded);
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
        fn restored_images_page_generates_actual_output_and_native_clipboard_pixels_attach(
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
            cx.update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| {
                    assert_eq!(view.page, Page::Images);
                    view.image_generation.prompt.update(cx, |state, cx| {
                        state.set_value("A native fixture image", window, cx)
                    });
                });
                window.render_frame(cx);
                window.click("generate-image", cx);
                assert!(view.read(cx).image_generation.busy());
            })
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
            cx.update_window(window, |_, window, cx| {
                window.render_frame(cx);
                window.click("attach-generated-image", cx);
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
                window.click("paste-image-files", cx);
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
