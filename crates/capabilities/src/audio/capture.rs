//! Input callbacks only mix samples and write a fixed lock-free SPSC ring.
//! Device creation, file I/O and conversion stay on the owned recording thread.
use super::{RecordingStatus, convert};
use anyhow::{Context, Result, ensure};
use cpal::{
    FromSample, Sample, SampleFormat, SizedSample,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use rtrb::{Producer, RingBuffer};
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

pub(super) const SAVE: u8 = 1;
pub(super) const DISCARD: u8 = 2;
pub(super) struct Capture {
    pub id: uuid::Uuid,
    pub command: Arc<AtomicU8>,
    pub status: Arc<Mutex<RecordingStatus>>,
}
impl Capture {
    pub fn snapshot(&self) -> Result<RecordingStatus> {
        self.status
            .lock()
            .map(|status| status.clone())
            .map_err(|_| anyhow::anyhow!("Recording state unavailable"))
    }
    pub fn active(&self) -> bool {
        self.snapshot().is_ok_and(|status| status.active())
    }
}

pub(super) fn spawn(
    root: PathBuf,
    max_seconds: u32,
    device_id: Option<String>,
) -> (
    Capture,
    oneshot::Receiver<std::result::Result<RecordingStatus, String>>,
) {
    let id = uuid::Uuid::new_v4();
    let status = Arc::new(Mutex::new(RecordingStatus::new(id, max_seconds)));
    let command = Arc::new(AtomicU8::new(0));
    let capture = Capture {
        id,
        command: command.clone(),
        status: status.clone(),
    };
    let (ready, receiver) = oneshot::channel();
    std::thread::spawn(move || {
        let mut ready = Some(ready);
        let result=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||record(root,max_seconds,device_id,&command,&status,&mut ready))).unwrap_or_else(|_|Err(anyhow::anyhow!("Native microphone operation stopped unexpectedly. Check the input device and try again.")));
        if let Err(error) = result {
            let message = format!("{error:#}");
            if let Some(ready) = ready.take() {
                let _ = ready.send(Err(message.clone()));
            }
            if let Ok(mut status) = status.lock() {
                status.state = if command.load(Ordering::Acquire) == DISCARD {
                    "cancelled"
                } else {
                    "failed"
                }
                .into();
                status.message = Some(message);
                status.path = None;
            }
        }
    });
    (capture, receiver)
}

struct RemoveOnDrop(PathBuf);
impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn record(
    root: PathBuf,
    max_seconds: u32,
    device_id: Option<String>,
    command: &Arc<AtomicU8>,
    status: &Arc<Mutex<RecordingStatus>>,
    ready: &mut Option<oneshot::Sender<std::result::Result<RecordingStatus, String>>>,
) -> Result<()> {
    ensure!(
        command.load(Ordering::Acquire) == 0,
        "Microphone request cancelled before opening the device"
    );
    let host = cpal::default_host();
    let device=if let Some(id)=device_id{host.device_by_id(&id.parse().context("Invalid audio device ID")?)}else{host.default_input_device()}.context("No microphone is available. Connect an input device and enable microphone access in system privacy settings.")?;
    let supported=device.default_input_config().context("Microphone cannot be opened. Enable access in system privacy settings and close applications holding the device.")?;
    let rate = supported.sample_rate();
    let channels = supported.channels();
    let format = supported.sample_format();
    ensure!(
        (8000..=192000).contains(&rate) && (1..=8).contains(&channels),
        "This input device requires an unsupported rate/channel layout; choose another microphone"
    );
    ensure!(
        command.load(Ordering::Acquire) == 0,
        "Microphone request cancelled before starting capture"
    );
    let (producer, mut consumer) = RingBuffer::<f32>::new(65536);
    let dropped = Arc::new(AtomicU64::new(0));
    let peak = Arc::new(AtomicU32::new(0));
    let (errors, events) = std::sync::mpsc::sync_channel::<String>(1);
    let stream=match format {
        SampleFormat::F32=>stream::<f32>(&device,supported.into(),producer,channels,command.clone(),dropped.clone(),peak.clone(),errors),
        SampleFormat::F64=>stream::<f64>(&device,supported.into(),producer,channels,command.clone(),dropped.clone(),peak.clone(),errors),
        SampleFormat::I16=>stream::<i16>(&device,supported.into(),producer,channels,command.clone(),dropped.clone(),peak.clone(),errors),
        SampleFormat::I32=>stream::<i32>(&device,supported.into(),producer,channels,command.clone(),dropped.clone(),peak.clone(),errors),
        SampleFormat::U16=>stream::<u16>(&device,supported.into(),producer,channels,command.clone(),dropped.clone(),peak.clone(),errors),
        SampleFormat::I8=>stream::<i8>(&device,supported.into(),producer,channels,command.clone(),dropped.clone(),peak.clone(),errors),
        SampleFormat::U8=>stream::<u8>(&device,supported.into(),producer,channels,command.clone(),dropped.clone(),peak.clone(),errors),
        SampleFormat::U32=>stream::<u32>(&device,supported.into(),producer,channels,command.clone(),dropped.clone(),peak.clone(),errors),
        _=>anyhow::bail!("This microphone sample format is unsupported; choose a PCM input device"),
    }.context("Microphone access failed. Enable Xylo in system microphone privacy settings and try Record again.")?;
    ensure!(
        command.load(Ordering::Acquire) == 0,
        "Microphone request cancelled before starting capture"
    );
    let id = status
        .lock()
        .map_err(|_| anyhow::anyhow!("Recording state unavailable"))?
        .recording_id;
    let temporary = RemoveOnDrop(root.join(format!(".recording-{id}.wav")));
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary.0)?;
    let mut writer = hound::WavWriter::new(
        std::io::BufWriter::new(file),
        hound::WavSpec {
            channels: 1,
            sample_rate: rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        },
    )?;
    stream.play().context(
        "Start microphone capture; check system privacy settings and the selected device",
    )?;
    {
        let mut state = status
            .lock()
            .map_err(|_| anyhow::anyhow!("Recording state unavailable"))?;
        state.state = "recording".into();
        state.sample_rate = rate;
        state.channels = channels;
        state.message = None;
        if let Some(ready) = ready.take() {
            let _ = ready.send(Ok(state.clone()));
        }
    }
    let began = Instant::now();
    let limit = u64::from(rate) * u64::from(max_seconds);
    let mut frames = 0u64;
    loop {
        if let Ok(error) = events.try_recv() {
            anyhow::bail!(
                "Microphone disconnected or failed: {error}. Select an available input device and record again."
            );
        }
        ensure!(
            dropped.load(Ordering::Relaxed) == 0,
            "Audio capture could not keep up; no incomplete recording was saved. Close busy applications and try again."
        );
        while let Ok(sample) = consumer.pop() {
            if frames < limit {
                writer.write_sample((sample.clamp(-1., 1.) * 32767.).round() as i16)?;
                frames += 1;
            }
        }
        if let Ok(mut state) = status.lock() {
            state.elapsed_ms = began.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
            state.peak = f32::from_bits(peak.load(Ordering::Relaxed));
            state.dropped_frames = dropped.load(Ordering::Relaxed);
        }
        if command.load(Ordering::Acquire) != 0
            || began.elapsed() >= Duration::from_secs(u64::from(max_seconds))
            || frames >= limit
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(stream);
    ensure!(
        dropped.load(Ordering::Relaxed) == 0,
        "Audio capture could not keep up; no incomplete recording was saved. Close busy applications and try again."
    );
    if command.load(Ordering::Acquire) == DISCARD {
        anyhow::bail!("Recording cancelled and discarded");
    }
    while let Ok(sample) = consumer.pop() {
        if frames < limit {
            writer.write_sample((sample.clamp(-1., 1.) * 32767.).round() as i16)?;
            frames += 1;
        }
    }
    writer.finalize()?;
    ensure!(
        frames > 0,
        "No microphone audio was captured. Check the input device and microphone permission, then try again."
    );
    if let Ok(mut state) = status.lock() {
        state.state = "finalizing".into();
    }
    let path = root.join(format!("recording-{id}.wav"));
    let token = CancellationToken::new();
    // Conversion checks cancellation through a short-lived watcher; this is
    // outside the realtime callback and releases on normal completion.
    let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let done = finished.clone();
    let stop = command.clone();
    let cancel = token.clone();
    let watcher = std::thread::spawn(move || {
        while !done.load(Ordering::Acquire) {
            if stop.load(Ordering::Acquire) == DISCARD {
                cancel.cancel();
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    });
    let result = std::fs::File::open(&temporary.0)
        .map_err(anyhow::Error::from)
        .and_then(|input| convert::normalize(input, &path, max_seconds, &token));
    finished.store(true, Ordering::Release);
    let _ = watcher.join();
    let info = result?;
    if command.load(Ordering::Acquire) == DISCARD {
        let _ = std::fs::remove_file(path);
        anyhow::bail!("Recording cancelled and discarded");
    }
    let mut state = status
        .lock()
        .map_err(|_| anyhow::anyhow!("Recording state unavailable"))?;
    state.state = "recorded".into();
    state.path = Some(info.path);
    state.sample_rate = info.sample_rate;
    state.channels = info.channels;
    state.duration_seconds = Some(info.duration_seconds);
    state.elapsed_ms = (info.duration_seconds * 1000.).round() as u64;
    state.peak = 0.;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn stream<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    mut producer: Producer<f32>,
    channels: u16,
    command: Arc<AtomicU8>,
    dropped: Arc<AtomicU64>,
    peak: Arc<AtomicU32>,
    errors: std::sync::mpsc::SyncSender<String>,
) -> std::result::Result<cpal::Stream, cpal::Error>
where
    T: SizedSample + Copy,
    f32: FromSample<T>,
{
    device.build_input_stream(
        config,
        move |samples: &[T], _| {
            if command.load(Ordering::Relaxed) != 0 {
                return;
            }
            let mut maximum = 0f32;
            for frame in samples.chunks_exact(usize::from(channels)) {
                let sample = frame
                    .iter()
                    .map(|&sample| {
                        let value = f32::from_sample(sample);
                        if value.is_finite() {
                            value.clamp(-1., 1.)
                        } else {
                            0.
                        }
                    })
                    .sum::<f32>()
                    / f32::from(channels);
                maximum = maximum.max(sample.abs());
                if producer.push(sample).is_err() {
                    dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
            peak.store(maximum.to_bits(), Ordering::Relaxed);
        },
        move |error| {
            let _ = errors.try_send(error.to_string().chars().take(2048).collect());
        },
        Some(Duration::from_secs(5)),
    )
}
