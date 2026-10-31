//! Playback owns its output stream until completion/cancellation. Merely
//! inspecting service status never initializes an audio device.
use super::convert::{MAX_AUDIO_FILE, MAX_SECONDS};
use anyhow::{Context, Result, ensure};
use rodio::{Decoder, Source};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

/// Plays the already opened `file` (shown as `path`).
pub(super) fn play(
    path: &Path,
    file: std::fs::File,
    volume: f32,
    cancel: &CancellationToken,
) -> Result<Value> {
    ensure!(
        volume.is_finite() && (0.0..=1.0).contains(&volume),
        "Volume must be between 0 and 1"
    );
    ensure!(
        file.metadata()?.len() <= MAX_AUDIO_FILE,
        "Audio file is missing or exceeds 64MiB"
    );
    ensure!(
        !cancel.is_cancelled(),
        "Playback cancelled before opening an audio device"
    );
    let decoder = Decoder::try_from(file)
        .context("Audio format cannot be played; use WAV, MP3, M4A, FLAC or Ogg")?;
    let duration = decoder.total_duration();
    if let Some(duration) = duration {
        ensure!(
            duration.as_secs_f64() <= f64::from(MAX_SECONDS) + 0.1,
            "Playback exceeds the 120-second audio limit"
        );
    }
    let failed = Arc::new(AtomicBool::new(false));
    let fault = failed.clone();
    let mut output = rodio::DeviceSinkBuilder::from_default_device()
        .context("No audio output is available. Select an output device in system sound settings.")?
        .with_error_callback(move |_| {
            fault.store(true, Ordering::Release);
        })
        .open_sink_or_fallback()
        .context("Audio output could not start. Check the device and system sound settings.")?;
    output.log_on_drop(false);
    let player = rodio::Player::connect_new(output.mixer());
    player.set_volume(volume);
    player.append(decoder.take_duration(Duration::from_secs(u64::from(MAX_SECONDS))));
    let began = Instant::now();
    while !player.empty() {
        if cancel.is_cancelled() {
            player.stop();
            anyhow::bail!("Audio playback cancelled");
        }
        if failed.load(Ordering::Acquire) {
            player.stop();
            anyhow::bail!(
                "Audio output disconnected. Select an available output device and try Play again."
            );
        }
        if began.elapsed() > Duration::from_secs(u64::from(MAX_SECONDS) + 3) {
            player.stop();
            anyhow::bail!("Audio output exceeded its playback deadline");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(
        json!({"state":"played","path":path,"duration_seconds":duration.map(|duration|duration.as_secs_f64()),"volume":volume}),
    )
}

#[cfg(test)]
mod tests;
