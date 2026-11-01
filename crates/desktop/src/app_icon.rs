//! Install the application icon for both bundled and `cargo run` launches.
//!
//! Window icons are an X11 facility; macOS displays NSApplication's icon in
//! the Dock and application switcher instead.
use anyhow::{Context, Result};
use objc2::{AnyThread, MainThreadMarker};
use objc2_app_kit::{NSApplication, NSImage};
use objc2_foundation::{NSData, NSSize};

pub fn install() -> Result<()> {
    // GPUI calls this from its application startup callback on the main thread.
    let main_thread = MainThreadMarker::new().context("Dock icon requires the main thread")?;
    let mut png = std::io::Cursor::new(Vec::new());
    crate::ui::brand_pixels(512)
        .write_to(&mut png, image::ImageFormat::Png)
        .context("Encode the Xylo application icon")?;
    let data = NSData::from_vec(png.into_inner());
    let icon = NSImage::initWithData(NSImage::alloc(), &data)
        .context("Decode the Xylo application icon")?;
    icon.setSize(NSSize::new(256., 256.));
    let application = NSApplication::sharedApplication(main_thread);
    // SAFETY: Called on the AppKit main thread with a valid NSImage. AppKit
    // retains the image, so the local retained reference may be dropped.
    unsafe { application.setApplicationIconImage(Some(&icon)) };
    Ok(())
}
