//! Actual Cmd/Ctrl+V -> temporary Host import -> production composer GPU captures.
//! Test-only imported production modules also expose fixtures for other targets.
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
        pub(crate) fn paste_ready(&self) -> bool {
            self.settings_loaded
                && self.workspace_ready
                && self
                    .drafts
                    .validation_revision(&self.current_draft_key())
                    .is_some()
        }
        pub(crate) fn paste_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
            self.composer.update(cx, |input, cx| {
                input.set_value("Please review these pasted images.", window, cx);
                input.focus(window, cx);
            });
            self.draft_changed(cx);
        }
        pub(crate) fn paste_count(&self) -> usize {
            self.draft_attachments().len()
        }
        pub(crate) fn paste_settled(&self) -> bool {
            !self.attachments.importing_for(self.session)
        }
        pub(crate) fn paste_theme(&self) -> &str {
            &self.settings.theme
        }
        pub(crate) fn paste_images(&self) -> Vec<ImageAttachment> {
            self.draft_attachments()
        }
        pub(crate) fn paste_healthy(&self) {
            assert!(self.error.is_none(), "Isolated Host reported an app error");
            assert!(
                !self.running() && !self.speech.busy(),
                "Capture must not start a model or device"
            );
        }
    }
}
fn main() {
    use gpui_kit::{
        component::{Theme, ThemeMode},
        prelude::*,
        test::{TestQueryExt, TestWindowExt},
        *,
    };
    use std::{
        path::PathBuf,
        sync::Arc,
        time::{Duration, Instant},
    };
    use xylo_core::{AppSettings, AttachmentSource};
    use xylo_runtime::{AppPaths, host::HostClient};
    use xylo_services::Store;
    let temporary = tempfile::tempdir().unwrap();
    let workspace = temporary.path().join("Paste fixture");
    std::fs::create_dir(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();
    let image = image::RgbaImage::from_fn(160, 100, |x, y| {
        if (x / 20 + y / 20) % 2 == 0 {
            image::Rgba([167, 184, 240, 255])
        } else {
            image::Rgba([238, 240, 246, 255])
        }
    });
    let mut raw = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut raw, image::ImageFormat::Png)
        .unwrap();
    let copied = workspace.join("copied-image.png");
    let file = image::RgbaImage::from_fn(160, 100, |x, y| {
        if x > y {
            image::Rgba([93, 139, 148, 255])
        } else {
            image::Rgba([229, 234, 237, 255])
        }
    });
    file.save(&copied).unwrap();
    let paths = AppPaths::at(temporary.path().join("profile")).unwrap();
    let store = Store::open(&paths.database).unwrap();
    let mut settings = AppSettings {
        last_workspace: Some(workspace.clone()),
        theme: "light".into(),
        ..Default::default()
    };
    settings.provider.model = "fixture-display-only".into();
    settings.remember_model(settings.provider.clone());
    store.save_settings(&settings).unwrap();
    let host = HostClient::spawn_at(paths).unwrap();
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
    let wait = |cx: &mut HeadlessAppContext, count: Option<usize>, theme: Option<&str>| {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            cx.advance_clock(Duration::from_millis(25));
            cx.run_until_parked();
            if cx.update(|cx| {
                let view = view.read(cx);
                view.paste_healthy();
                view.paste_ready()
                    && count.is_none_or(|count| view.paste_count() == count && view.paste_settled())
                    && theme.is_none_or(|theme| view.paste_theme() == theme)
            }) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "Timed out waiting for isolated paste fixture"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    wait(&mut cx, None, Some("light"));
    let shortcut = if cfg!(target_os = "macos") {
        "cmd-v"
    } else {
        "ctrl-v"
    };
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |view, cx| view.paste_focus(window, cx));
        cx.write_to_clipboard(ClipboardItem::new_image(&Image::from_bytes(
            ImageFormat::Png,
            raw.into_inner(),
        )));
        window.render_frame(cx);
        window.press(shortcut, cx);
    })
    .unwrap();
    wait(&mut cx, Some(1), None);
    cx.update_window(handle, |_, window, cx| {
        cx.write_to_clipboard(ClipboardItem::from(ClipboardEntry::ExternalPaths(
            ExternalPaths(vec![copied].into()),
        )));
        window.press(shortcut, cx);
    })
    .unwrap();
    wait(&mut cx, Some(2), None);
    let images = cx.update(|cx| view.read(cx).paste_images());
    assert_eq!(images[0].source, AttachmentSource::Clipboard);
    assert_eq!(images[1].source, AttachmentSource::PastedFile);
    let output = std::env::var_os("XYLO_RENDER_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../../.scratch/ui-fixes/paste-captures")
        });
    std::fs::create_dir_all(&output).unwrap();
    let mut cases = Vec::new();
    for theme in ["light", "dark"] {
        settings.theme = theme.into();
        client.save_settings(settings.clone()).unwrap();
        wait(&mut cx, Some(2), Some(theme));
        for (width, height) in [(1000, 680), (1380, 880)] {
            cx.update_window(handle, |_, window, cx| {
                window.resize(size(px(width as f32), px(height as f32)));
                window.bounds_changed(cx);
                window.render_frame(cx);
            })
            .unwrap();
            for _ in 0..8 {
                cx.advance_clock(Duration::from_millis(25));
                cx.run_until_parked();
                cx.update_window(handle, |_, window, cx| window.render_frame(cx))
                    .unwrap();
                std::thread::sleep(Duration::from_millis(5));
            }
            let geometry=cx.update_window(handle,|_,window,cx|{
                view.read(cx).paste_healthy();let input=window.find_by_label("Message").bounds();
                for image in &images {
                    let button=window.find(SharedString::from(format!("remove-image-{}",image.id)));let bounds=button.bounds();
                    assert!(button.visible()&&bounds.left()>=input.left()-px(4.)&&bounds.right()<=input.right()+px(4.)&&bounds.bottom()<=px(height as f32),"Pasted image controls must remain visible inside composer");
                }
                serde_json::json!({"input_bounds":[f32::from(input.left()),f32::from(input.top()),f32::from(input.size.width),f32::from(input.size.height)]})
            }).unwrap();
            let filename = format!("paste-{theme}-{width}x{height}.png");
            assert!(
                !output.join(&filename).exists(),
                "Do not overwrite prior capture evidence"
            );
            cx.capture_screenshot(handle)
                .unwrap()
                .save(output.join(&filename))
                .unwrap();
            cases.push(serde_json::json!({"theme":theme,"viewport":[width,height],"image":filename,"geometry":geometry}));
        }
    }
    use sha2::Digest;
    let compiled_sources = [
        ("chat.rs", include_bytes!("../src/xylo/chat.rs").as_slice()),
        ("attachments.rs", include_bytes!("../src/xylo/attachments.rs").as_slice()),
        ("paste_rendering.rs", include_bytes!("paste_rendering.rs").as_slice()),
        ("Cargo.lock", include_bytes!("Cargo.lock").as_slice()),
    ].into_iter().map(|(name, bytes)| (name, format!("{:x}", sha2::Sha256::digest(bytes)))).collect::<std::collections::BTreeMap<_, _>>();
    let metadata = serde_json::json!({"schema":1,"scope":"Current production composer, actual native paste shortcut and real isolated Host imports; native GPU/fonts. No account, model, real profile, or device.","clipboard_sources":["clipboard","pasted_file"],"attachment_count":2,"cases":cases,"compiled_sources":compiled_sources});
    std::fs::write(
        output.join("paste-rendering.json"),
        serde_json::to_vec_pretty(&metadata).unwrap(),
    )
    .unwrap();
    client.shutdown().unwrap();
    cx.run_until_parked();
    println!("Paste captures: {}", output.display());
}
