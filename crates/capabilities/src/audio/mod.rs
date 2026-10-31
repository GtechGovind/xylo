//! Native speech I/O exposed only as private helper controls. Capture requires
//! an explicit user action; these controls are deliberately absent from the LLM
//! tool catalog. Device/model initialization is lazy.
mod capture;
pub mod convert;
mod playback;

use crate::{CapabilityConfig, input_path};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex, atomic::Ordering},
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecordingStatus {
    pub recording_id: Uuid,
    pub state: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub max_seconds: u32,
    pub elapsed_ms: u64,
    pub peak: f32,
    pub dropped_frames: u64,
    pub path: Option<PathBuf>,
    pub duration_seconds: Option<f64>,
    pub message: Option<String>,
}
impl RecordingStatus {
    fn new(id: Uuid, max_seconds: u32) -> Self {
        Self {
            recording_id: id,
            state: "starting".into(),
            sample_rate: 0,
            channels: 0,
            max_seconds,
            elapsed_ms: 0,
            peak: 0.,
            dropped_frames: 0,
            path: None,
            duration_seconds: None,
            message: None,
        }
    }
    fn active(&self) -> bool {
        matches!(self.state.as_str(), "starting" | "recording" | "finalizing")
    }
}
struct Inner {
    config: CapabilityConfig,
    capture: Mutex<Option<capture::Capture>>,
    playback: Mutex<Option<(Uuid, CancellationToken)>>,
}
impl Drop for Inner {
    fn drop(&mut self) {
        if let Ok(capture) = self.capture.lock()
            && let Some(capture) = capture.as_ref()
        {
            capture.command.store(capture::DISCARD, Ordering::Release);
        }
        if let Ok(playback) = self.playback.lock()
            && let Some((_, cancel)) = playback.as_ref()
        {
            cancel.cancel();
        }
    }
}
#[derive(Clone)]
pub struct AudioService(Arc<Inner>);
impl AudioService {
    /// Validates roots only. It does not enumerate/open microphones or outputs.
    pub fn new(workspace: &Path, artifacts: &Path) -> Result<Self> {
        let workspace = std::fs::canonicalize(workspace)?;
        let artifacts = std::fs::canonicalize(artifacts)?;
        ensure!(
            workspace.is_dir() && artifacts.is_dir(),
            "Audio roots must be existing directories"
        );
        let config = CapabilityConfig {
            workspace,
            artifacts,
            ..Default::default()
        };
        Ok(Self(Arc::new(Inner {
            config,
            capture: Mutex::new(None),
            playback: Mutex::new(None),
        })))
    }
    pub fn active_jobs(&self) -> usize {
        let capture = self
            .0
            .capture
            .lock()
            .map(|capture| usize::from(capture.as_ref().is_some_and(capture::Capture::active)))
            .unwrap_or(1);
        let playback = self
            .0
            .playback
            .lock()
            .map(|playback| usize::from(playback.is_some()))
            .unwrap_or(1);
        capture + playback
    }
    pub fn status(&self) -> Result<Value> {
        let recording = self.recording_status()?;
        let playing = self
            .0
            .playback
            .lock()
            .map_err(|_| anyhow::anyhow!("Audio state unavailable"))?
            .is_some();
        Ok(
            json!({"recording":recording,"playing":playing,"capture_requires_user_action":true,"max_seconds":convert::MAX_SECONDS,"device_status":"not_probed"}),
        )
    }
    fn recording_status(&self) -> Result<Value> {
        let capture = self
            .0
            .capture
            .lock()
            .map_err(|_| anyhow::anyhow!("Recording state unavailable"))?;
        match capture.as_ref() {
            Some(capture) => Ok(serde_json::to_value(capture.snapshot()?)?),
            None => Ok(json!({"state":"idle"})),
        }
    }
    pub async fn execute(
        &self,
        name: &str,
        args: &Value,
        cancel: CancellationToken,
    ) -> Result<Value> {
        ensure!(!cancel.is_cancelled(), "Audio operation cancelled");
        match name {
            "helper_audio_status" => self.status(),
            "helper_audio_devices" => tokio::task::spawn_blocking(devices).await?,
            "helper_audio_record_status" => self.recording_status(),
            "helper_audio_record_start" => {
                ensure!(
                    args.get("user_initiated").and_then(Value::as_bool) == Some(true),
                    "Microphone recording requires an explicit user Record action"
                );
                let duration = args
                    .get("max_seconds")
                    .map(|v| v.as_u64().context("Recording duration must be an integer"))
                    .transpose()?
                    .unwrap_or(u64::from(convert::MAX_SECONDS));
                ensure!(
                    (1..=u64::from(convert::MAX_SECONDS)).contains(&duration),
                    "Recording duration must be 1–120 seconds"
                );
                let device_id = args
                    .get("device_id")
                    .filter(|value| !value.is_null())
                    .map(|value| {
                        value
                            .as_str()
                            .context("Device ID must be text")
                            .map(str::to_owned)
                    })
                    .transpose()?;
                if let Some(id) = device_id.as_ref() {
                    ensure!(id.len() <= 1024, "Device ID exceeds budget");
                }
                let (id, command, ready) = {
                    let playback = self
                        .0
                        .playback
                        .lock()
                        .map_err(|_| anyhow::anyhow!("Audio state unavailable"))?;
                    ensure!(
                        playback.is_none(),
                        "Stop audio playback before recording to avoid recording the speakers"
                    );
                    let mut current = self
                        .0
                        .capture
                        .lock()
                        .map_err(|_| anyhow::anyhow!("Recording state unavailable"))?;
                    ensure!(
                        !current.as_ref().is_some_and(capture::Capture::active),
                        "A recording is already active"
                    );
                    let (capture, ready) =
                        capture::spawn(self.0.config.artifacts.clone(), duration as u32, device_id);
                    let result = (capture.id, capture.command.clone(), ready);
                    *current = Some(capture);
                    result
                };
                let result = tokio::select! {result=ready=>result.context("Microphone startup stopped unexpectedly")?.map_err(anyhow::Error::msg),_=cancel.cancelled()=>Err(anyhow::anyhow!("Microphone startup cancelled")),_=tokio::time::sleep(Duration::from_secs(30))=>Err(anyhow::anyhow!("Microphone permission/device did not respond. Check system privacy settings and try Record again."))};
                match result {
                    Ok(status) => Ok(serde_json::to_value(status)?),
                    Err(error) => {
                        command.store(capture::DISCARD, Ordering::Release);
                        Err(error.context(format!("Recording {id} did not start")))
                    }
                }
            }
            "helper_audio_record_stop" | "helper_audio_record_cancel" => {
                let id = args
                    .get("recording_id")
                    .map(|value| serde_json::from_value::<Uuid>(value.clone()))
                    .transpose()?;
                ensure!(
                    name.ends_with("cancel") || id.is_some(),
                    "Recording ID is required"
                );
                let command = {
                    let capture = self
                        .0
                        .capture
                        .lock()
                        .map_err(|_| anyhow::anyhow!("Recording state unavailable"))?;
                    let Some(capture) = capture.as_ref() else {
                        return Ok(json!({"state":"idle"}));
                    };
                    ensure!(
                        id.is_none_or(|id| capture.id == id),
                        "Recording ID is no longer current"
                    );
                    capture.command.clone()
                };
                command.store(
                    if name.ends_with("cancel") {
                        capture::DISCARD
                    } else {
                        capture::SAVE
                    },
                    Ordering::Release,
                );
                let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
                loop {
                    let status = self.recording_status()?;
                    if !matches!(
                        status["state"].as_str(),
                        Some("starting" | "recording" | "finalizing")
                    ) {
                        if status["state"] == "failed" {
                            anyhow::bail!(
                                "{}",
                                status["message"]
                                    .as_str()
                                    .unwrap_or("Microphone capture failed")
                            );
                        }
                        return Ok(status);
                    }
                    tokio::select! {_=cancel.cancelled()=>{command.store(capture::DISCARD,Ordering::Release);anyhow::bail!("Recording stop cancelled; audio is being discarded");},_=tokio::time::sleep_until(deadline)=>{command.store(capture::DISCARD,Ordering::Release);anyhow::bail!("Microphone did not stop within its deadline; close Xylo to release the device");},_=tokio::time::sleep(Duration::from_millis(20))=>{}}
                }
            }
            "helper_audio_play" => {
                ensure!(
                    args.get("user_initiated").and_then(Value::as_bool) == Some(true),
                    "Audio playback requires an explicit user Play action"
                );
                let path = input_path(&self.0.config, crate::required_str(args, "path")?)?;
                ensure!(
                    path.starts_with(&self.0.config.artifacts),
                    "Playback requires a recording or generated audio in Xylo artifacts"
                );
                let volume = args
                    .get("volume")
                    .map(|value| value.as_f64().context("Volume must be a number"))
                    .transpose()?
                    .unwrap_or(0.8);
                ensure!(
                    volume.is_finite() && (0.0..=1.0).contains(&volume),
                    "Volume must be between 0 and 1"
                );
                let id = Uuid::new_v4();
                let stop = cancel.child_token();
                {
                    let mut playback = self
                        .0
                        .playback
                        .lock()
                        .map_err(|_| anyhow::anyhow!("Audio state unavailable"))?;
                    ensure!(playback.is_none(), "Audio is already playing");
                    let capture = self
                        .0
                        .capture
                        .lock()
                        .map_err(|_| anyhow::anyhow!("Recording state unavailable"))?;
                    ensure!(
                        !capture.as_ref().is_some_and(capture::Capture::active),
                        "Stop recording before playing audio"
                    );
                    *playback = Some((id, stop.clone()));
                }
                let service = self.clone();
                tokio::task::spawn_blocking(move || {
                    struct PlaybackGuard(AudioService, Uuid);
                    impl Drop for PlaybackGuard {
                        fn drop(&mut self) {
                            if let Ok(mut playback) = self.0.0.playback.lock()
                                && playback.as_ref().is_some_and(|(id, _)| *id == self.1)
                            {
                                *playback = None;
                            }
                        }
                    }
                    let _guard = PlaybackGuard(service, id);
                    playback::play(&path, volume as f32, &stop)
                })
                .await?
            }
            "helper_audio_play_stop" => {
                let playback = self
                    .0
                    .playback
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Audio state unavailable"))?;
                if let Some((_, cancel)) = playback.as_ref() {
                    cancel.cancel();
                }
                Ok(json!({"state":"stopping"}))
            }
            _ => anyhow::bail!("Unknown native audio control"),
        }
    }
    pub async fn shutdown(&self) {
        if let Ok(capture) = self.0.capture.lock()
            && let Some(capture) = capture.as_ref()
        {
            capture.command.store(capture::DISCARD, Ordering::Release);
        }
        if let Ok(playback) = self.0.playback.lock()
            && let Some((_, cancel)) = playback.as_ref()
        {
            cancel.cancel();
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while self.active_jobs() > 0 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

fn devices() -> Result<Value> {
    use cpal::traits::{DeviceTrait, HostTrait};
    let host = cpal::default_host();
    let mut inputs = Vec::new();
    for device in host.input_devices()?.take(32) {
        inputs.push(json!({"id":device.id()?.to_string(),"name":device.description()?.name()}));
    }
    Ok(
        json!({"inputs":inputs,"default_input":host.default_input_device().and_then(|device|device.id().ok()).map(|id|id.to_string()),"output_available":host.default_output_device().is_some()}),
    )
}

#[cfg(test)]
mod tests;
