//! OS tray callbacks are forwarded to the GPUI dispatcher through a bounded
//! event channel. There is no timer or idle polling in this lifecycle path.
use anyhow::Result;
use tray_icon::{
    Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
};
#[derive(Clone, Copy)]
pub enum Event {
    Show,
    Quit,
}
pub fn create() -> Result<(TrayIcon, async_channel::Receiver<Event>)> {
    let menu = Menu::new();
    let show = MenuItem::new("Open Xylo", true, None);
    let quit = MenuItem::new("Quit Xylo", true, None);
    menu.append(&show)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&quit)?;
    let (show_id, quit_id) = (show.id().clone(), quit.id().clone());
    let (tx, rx) = async_channel::bounded(8);
    let menu_tx = tx.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let action = if event.id == show_id {
            Some(Event::Show)
        } else if event.id == quit_id {
            Some(Event::Quit)
        } else {
            None
        };
        if let Some(action) = action {
            let _ = menu_tx.try_send(action);
        }
    }));
    TrayIconEvent::set_event_handler(Some(move |event| {
        if matches!(
            event,
            TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            }
        ) {
            let _ = tx.try_send(Event::Show);
        }
    }));
    let image = tray_icon()?;
    let mut builder = TrayIconBuilder::new()
        .with_tooltip("Xylo")
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false);
    #[cfg(target_os = "macos")]
    {
        builder = builder.with_icon_templated(image);
    }
    #[cfg(not(target_os = "macos"))]
    {
        builder = builder.with_icon(image);
    }
    Ok((builder.build()?, rx))
}
/// macOS receives only the mark as an alpha template; other platforms use
/// the rounded application tile. Transparent corners must stay transparent.
fn tray_image() -> image::RgbaImage {
    #[cfg(target_os = "macos")]
    {
        tray_template_image()
    }
    #[cfg(not(target_os = "macos"))]
    {
        crate::ui::brand_pixels(24)
    }
}

#[cfg(any(target_os = "macos", test))]
fn tray_template_image() -> image::RgbaImage {
    // Extract the mark before resampling; filtering the indigo tile first
    // would blend its edge into the mark and erode the monochrome template.
    let mut image = crate::ui::brand_pixels(1024);
    for pixel in image.pixels_mut() {
        let alpha = if pixel.0[..3] == [255, 255, 255] {
            pixel.0[3]
        } else {
            0
        };
        *pixel = image::Rgba([0, 0, 0, alpha]);
    }
    image::imageops::resize(&image, 24, 24, image::imageops::FilterType::Lanczos3)
}

fn tray_icon() -> Result<Icon> {
    let image = tray_image();
    let (width, height) = image.dimensions();
    Ok(Icon::from_rgba(image.into_raw(), width, height)?)
}

#[cfg(test)]
mod tests {
    #[test]
    fn menu_bar_template_is_a_smooth_mark_with_no_background_tile() {
        let image = super::tray_template_image();
        assert_eq!(image.dimensions(), (24, 24));
        for edge in 0..24 {
            for (x, y) in [(edge, 0), (edge, 23), (0, edge), (23, edge)] {
                assert_eq!(
                    image.get_pixel(x, y).0[3],
                    0,
                    "Opaque menu-bar edge at {x},{y}"
                );
            }
        }
        let visible = image.pixels().filter(|pixel| pixel.0[3] > 127).count();
        assert!(
            (30..180).contains(&visible),
            "The icon must be a mark, not an opaque tile: {visible}"
        );
        assert!(image.pixels().any(|pixel| (1..255).contains(&pixel.0[3])));
        assert!(image.pixels().all(|pixel| pixel.0[..3] == [0, 0, 0]));
    }
}
