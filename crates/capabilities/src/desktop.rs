//! Native display capture and input. OS permissions are never inferred from
//! configuration; every operation reports its actual result.

use crate::{CapabilityConfig, definition, observation, output_path, required_str};
use anyhow::{Result, ensure};
use enigo::{Button, Coordinate, Direction, Enigo, Keyboard, Mouse, Settings};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use xylo_core::{ToolDefinition, ToolObservation};

pub fn availability() -> Value {
    let wayland = cfg!(target_os = "linux") && std::env::var_os("WAYLAND_DISPLAY").is_some();
    json!({"state":"permission_check_on_use","capture_backend":"xcap","input_backend":"enigo",
        "wayland":wayland,"input_restricted":wayland,"note":if wayland {"Wayland global input requires a compositor-supported permission mechanism; unrestricted injection is not assumed."} else {"macOS Screen Recording/Accessibility or corresponding OS permissions are required."}})
}

pub async fn execute(
    config: &CapabilityConfig,
    name: &str,
    args: &Value,
    cancel: CancellationToken,
) -> Result<ToolObservation> {
    let config = config.clone();
    let name = name.to_owned();
    let args = args.clone();
    ensure!(!cancel.is_cancelled(), "Desktop operation cancelled");
    tokio::task::spawn_blocking(move || {
        ensure!(!cancel.is_cancelled(), "Desktop operation cancelled");
        match name.as_str() {
            "desktop_displays" => {
                let monitors = xcap::Monitor::all()?;
                let displays: Vec<Value> = monitors.iter().map(|m| Ok(json!({"id":m.id()?,"name":m.name()?,"width":m.width()?,"height":m.height()?}))).collect::<Result<_>>()?;
                Ok(observation(json!({"displays":displays,"availability":availability()}), vec![]))
            },
            "desktop_screenshot" => {
                let monitors = xcap::Monitor::all()?;
                let id = args.get("display_id").and_then(Value::as_u64);
                let monitor = monitors.iter().find(|m| id.is_none_or(|id| m.id().ok().map(u64::from)==Some(id))).ok_or_else(||anyhow::anyhow!("Display not found"))?;
                let image = monitor.capture_image()?;
                ensure!(image.width() <= 16384 && image.height() <= 16384, "Display capture exceeds pixel budget");
                let path = output_path(&config, &format!("display-{}", uuid::Uuid::new_v4()), "png")?;
                image.save(&path)?;
                Ok(observation(json!({"display_id":monitor.id()?,"path":path,"width":image.width(),"height":image.height()}),vec![path]))
            },
            "desktop_click" | "desktop_type" => {
                ensure!(!(cfg!(target_os="linux") && std::env::var_os("WAYLAND_DISPLAY").is_some()),
                    "Global input is unavailable in this Wayland backend; use an authorized desktop portal/compositor integration");
                let mut enigo = Enigo::new(&Settings::default())?;
                if name == "desktop_type" {
                    let text = required_str(&args,"text")?; ensure!(text.len() <= 32768, "Desktop text exceeds limit");
                    enigo.text(text)?;
                } else {
                    let coordinate = |key| args.get(key).and_then(Value::as_i64).and_then(|v|i32::try_from(v).ok()).ok_or_else(||anyhow::anyhow!("Expected integer {key}"));
                    enigo.move_mouse(coordinate("x")?, coordinate("y")?, Coordinate::Abs)?;
                    enigo.button(Button::Left, Direction::Click)?;
                }
                Ok(observation(json!({"sent":true,"operation":name}),vec![]))
            },
            _ => anyhow::bail!("Unknown desktop tool"),
        }
    }).await?
}

pub fn definitions() -> Vec<ToolDefinition> {
    vec![
        definition(
            "desktop_displays",
            "List native displays and actual OS capture availability.",
            json!({}),
            &[],
            false,
        ),
        definition(
            "desktop_screenshot",
            "Capture a native display to a PNG artifact. OS screen-recording permission may be required.",
            json!({"display_id":{"type":"integer"}}),
            &[],
            true,
        ),
        definition(
            "desktop_click",
            "Move pointer to absolute OS coordinates and left click using native input. Requires OS permission.",
            json!({"x":{"type":"integer"},"y":{"type":"integer"}}),
            &["x", "y"],
            true,
        ),
        definition(
            "desktop_type",
            "Type text through native OS input into the currently focused app.",
            json!({"text":{"type":"string","maxLength":32768}}),
            &["text"],
            true,
        ),
    ]
}
