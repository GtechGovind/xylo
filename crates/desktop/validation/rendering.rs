//! Real Metal/WGPU layout captures with no Host, network or native client.
//! Plugins/Environments show controlled no-Host loading/setup snapshots. Loaded
//! catalogues, connections and outcomes are covered by management_interactions.
#![allow(dead_code, unused_imports)]
extern crate gpui_kit as gpui;
#[path = "../src/ui.rs"]
mod production_ui;
#[path = "../src/state.rs"]
mod state;
#[path = "../src/terminal_view.rs"]
mod terminal_view;
mod ui {
    pub use super::production_ui::*;
    use gpui_kit::{TestSupportExt, prelude::*, *};

    // Observe the existing heading without adding a layout node or changing its
    // pixels. This is harness-only accessibility instrumentation, not a claim
    // about the production heading's accessibility implementation.
    pub fn heading(
        title: impl Into<SharedString>,
        description: impl Into<SharedString>,
    ) -> AnyElement {
        let title = title.into();
        super::production_ui::heading(title.clone(), description)
            .id(SharedString::from(format!("capture-heading-{title}")))
            .role(Role::Heading)
            .aria_label(title)
            .test_support()
            .into_any_element()
    }
}
#[path = "../src/workbench.rs"]
mod workbench;
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
    use std::sync::Arc;
    let mut cx = HeadlessAppContext::with_platform(
        platform::current_platform(true).text_system(),
        Arc::new(gpui_kit::assets::Assets),
        platform::current_headless_renderer,
    );
    cx.update(|cx| {
        gpui_kit::init(cx);
        Theme::change(ThemeMode::Light, None, cx);
    });
    let (handle, view) = cx
        .update(|cx| {
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: Default::default(),
                        size: size(px(1380.), px(880.)),
                    })),
                    focus: false,
                    show: false,
                    ..Default::default()
                },
                cx,
                |window, cx| {
                    cx.new(|cx| {
                        workbench::Workbench::new(
                            Err(anyhow::anyhow!("Rendering preview")),
                            window,
                            cx,
                        )
                    })
                },
            )
        })
        .unwrap();
    view.update(&mut cx, |view, cx| view.prepare_render_preview(cx));
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find_by_label("New Chat").is_some());
        assert!(window.try_find_by_label("↑").is_some());
    })
    .unwrap();
    let output = std::env::var_os("WORKBENCH_RENDER_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("target/native-preview"));
    std::fs::create_dir_all(&output).unwrap();
    let home = cx
        .capture_screenshot(handle)
        .expect("Native renderer available");
    let mark_bounds = cx
        .update_window(handle, |_, window, _| {
            let mark = window.find("xylo-brand-mark");
            assert!(mark.visible(), "Xylo mark is clipped");
            mark.bounds()
        })
        .unwrap();
    assert_mark_pixels(&home, mark_bounds);
    home.save(output.join("home.png")).unwrap();
    cx.update_window(handle, |_, window, cx| {
        window.click("settings", cx);
        view.update(cx, |view, cx| view.prepare_render_preview(cx));
        window.render_frame(cx);
    })
    .unwrap();
    cx.capture_screenshot(handle)
        .unwrap()
        .save(output.join("settings.png"))
        .unwrap();
    for (navigation, title, action, filename) in [
        ("nav-14", "Plugins", "plugin-choose", "plugins.png"),
        (
            "nav-15",
            "Environments",
            "environment-new",
            "environments.png",
        ),
    ] {
        cx.update_window(handle, |_, window, cx| {
            window.click(navigation, cx);
            // No service exists in this harness. Suppress the expected service
            // error and retain the truthful loading state; never fabricate a
            // loaded catalogue, installed package or connected environment.
            view.update(cx, |view, cx| view.prepare_render_preview(cx));
            window.render_frame(cx);

            let heading = window.find(SharedString::from(format!("capture-heading-{title}")));
            let action = window.find(action);
            assert!(heading.visible(), "{title} heading is clipped");
            assert_eq!(heading.label(), Some(title));
            assert!(action.visible(), "{title} setup action is clipped");
            let plugins = window.find("nav-14");
            let environments = window.find("nav-15");
            assert!(plugins.visible() && environments.visible());
            assert_eq!(plugins.bounds().origin.x, environments.bounds().origin.x);
            assert_eq!(
                plugins.bounds().size.width,
                environments.bounds().size.width
            );
            assert!(heading.bounds().origin.x >= plugins.bounds().right());
            assert!(
                heading.bounds().origin.x <= action.bounds().origin.x + px(1.)
                    && heading.bounds().origin.x >= action.bounds().origin.x - px(1.),
                "{title} heading and primary action need the same leading alignment"
            );
            if title == "Plugins" {
                assert!(window.find("plugin-refresh").visible());
            } else {
                assert!(window.find("environment-refresh").visible());
                assert!(window.find_by_label("Name").visible());
                assert!(window.find("environment-client").visible());
            }
        })
        .unwrap();
        cx.capture_screenshot(handle)
            .expect("Native renderer available")
            .save(output.join(filename))
            .unwrap();
    }
    println!(
        "Native layout captures: {} (Plugins/Environments are controlled no-Host loading/setup snapshots; loaded states require the Host/helper fixtures)",
        output.display()
    );
}

/// Check the actual first-frame pixels, without executor waits that could hide
/// a startup race. A visible white tile alone is insufficient proof of a logo.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn assert_mark_pixels(image: &image::RgbaImage, bounds: gpui_kit::Bounds<gpui_kit::Pixels>) {
    let scale = image.width() as f32 / 1380.;
    let left = (f32::from(bounds.origin.x) * scale).round() as u32;
    let top = (f32::from(bounds.origin.y) * scale).round() as u32;
    let width = (f32::from(bounds.size.width) * scale).round() as u32;
    let height = (f32::from(bounds.size.height) * scale).round() as u32;
    assert!(width > 0 && height > 0);
    assert!(left + width <= image.width() && top + height <= image.height());
    let tile = image::imageops::crop_imm(image, left, top, width, height).to_image();
    let dark = tile
        .pixels()
        .filter(|pixel| pixel.0[..3].iter().all(|channel| *channel < 100))
        .count();
    let white = tile
        .pixels()
        .filter(|pixel| pixel.0[..3].iter().all(|channel| *channel > 240))
        .count();
    let area = (width * height) as usize;
    assert!(
        dark > area / 8,
        "The first frame must contain the Xylo mark"
    );
    assert!(white > area / 8, "The mark must retain its light tile");
    let cached = production_ui::brand_image();
    assert!(std::sync::Arc::ptr_eq(
        &cached,
        &production_ui::brand_image()
    ));
    let mut source = image::load_from_memory_with_format(
        include_bytes!("../../../packaging/icons/sidebar.png"),
        image::ImageFormat::Png,
    )
    .unwrap()
    .into_rgba8();
    for pixel in source.pixels_mut() {
        pixel.0.swap(0, 2);
    }
    assert_eq!(cached.as_bytes(0), Some(source.as_raw().as_slice()));
}
