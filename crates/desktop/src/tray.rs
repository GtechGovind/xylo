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
/// macOS receives the mark's alpha template. Other platforms receive its warm
/// white application tile so the mark remains visible on a dark system panel.
fn tray_icon() -> Result<Icon> {
    #[cfg(target_os = "macos")]
    let bytes = include_bytes!("../../../packaging/icons/tray.png").as_slice();
    #[cfg(not(target_os = "macos"))]
    let bytes = include_bytes!("../../../packaging/icons/app.png").as_slice();
    let image = image::load_from_memory_with_format(bytes, image::ImageFormat::Png)?.into_rgba8();
    #[cfg(not(target_os = "macos"))]
    let image = image::imageops::resize(&image, 24, 24, image::imageops::FilterType::Lanczos3);
    let (width, height) = image.dimensions();
    Ok(Icon::from_rgba(image.into_raw(), width, height)?)
}
