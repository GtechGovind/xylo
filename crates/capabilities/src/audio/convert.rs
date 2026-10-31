//! Bounded native decode, mono mixing and band-limited16kHz conversion.
use anyhow::{Context, Result, ensure};
use rodio::{Decoder, Source};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Indexing, Resampler, WindowFunction};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::BufWriter,
    path::{Path, PathBuf},
};
use tokio_util::sync::CancellationToken;

pub const TARGET_RATE: u32 = 16_000;
pub const MAX_SECONDS: u32 = 120;
pub const MAX_AUDIO_FILE: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AudioInfo {
    pub path: PathBuf,
    pub sample_rate: u32,
    pub channels: u16,
    pub frames: u64,
    pub duration_seconds: f64,
}

pub fn normalize(
    input: File,
    output: &Path,
    max_seconds: u32,
    cancel: &CancellationToken,
) -> Result<AudioInfo> {
    ensure!(
        input.metadata()?.len() <= MAX_AUDIO_FILE,
        "Audio file is missing or exceeds 64MiB"
    );
    let decoder =
        Decoder::try_from(input).context("Open supported WAV, MP3, M4A, FLAC or Ogg audio")?;
    let rate = decoder.sample_rate().get();
    let channels = decoder.channels().get();
    ensure!(
        (8000..=192000).contains(&rate) && (1..=8).contains(&channels),
        "Audio requires 8–192kHz PCM and 1–8 channels"
    );
    let max_seconds = max_seconds.clamp(1, MAX_SECONDS);
    if let Some(duration) = decoder.total_duration() {
        ensure!(
            duration.as_secs_f64() <= f64::from(max_seconds) + 0.1,
            "Audio exceeds the 120-second limit; choose a shorter clip"
        );
    }
    let limit = u64::from(rate) * u64::from(max_seconds);
    let mut mono = Vec::with_capacity((u64::from(rate) * 5).min(limit) as usize);
    let mut sum = 0f32;
    let mut channel = 0;
    for sample in decoder {
        if mono.len() % 4096 == 0 {
            ensure!(!cancel.is_cancelled(), "Audio conversion cancelled");
        }
        sum += if sample.is_finite() {
            sample.clamp(-1., 1.)
        } else {
            0.
        };
        channel += 1;
        if channel == channels {
            ensure!(
                (mono.len() as u64) < limit,
                "Audio exceeds the 120-second limit; choose a shorter clip"
            );
            mono.push(sum / f32::from(channels));
            sum = 0.;
            channel = 0;
        }
    }
    ensure!(
        channel == 0 && !mono.is_empty(),
        "Audio is empty or contains an incomplete frame"
    );
    let samples = resample(&mono, rate, TARGET_RATE, cancel)?;
    write_mono(output, &samples, TARGET_RATE, cancel)
}

pub(super) fn resample(
    samples: &[f32],
    input_rate: u32,
    output_rate: u32,
    cancel: &CancellationToken,
) -> Result<Vec<f32>> {
    ensure!(
        !samples.is_empty()
            && (8000..=192000).contains(&input_rate)
            && (8000..=192000).contains(&output_rate),
        "Invalid resampling input"
    );
    if input_rate == output_rate {
        return Ok(samples.to_vec());
    }
    // One sub-chunk keeps the bounded flush arithmetic below exact.
    let mut resampler = Fft::<f32>::new_custom(
        input_rate as usize,
        output_rate as usize,
        1024,
        1,
        1,
        WindowFunction::BlackmanHarris2,
        FixedSync::Input,
    )?;
    let expected =
        (samples.len() as f64 * f64::from(output_rate) / f64::from(input_rate)).ceil() as usize;
    let delay = resampler.output_delay();
    let capacity = resampler.process_all_needed_output_len(samples.len())
        + delay
        + resampler.output_frames_max();
    ensure!(
        capacity <= MAX_SECONDS as usize * 192000 + 65536,
        "Resampled audio exceeds memory budget"
    );
    let mut output = vec![0.; capacity];
    let input = InterleavedSlice::new(samples, 1, samples.len())?;
    let mut indexing = Indexing {
        input_offset: 0,
        output_offset: 0,
        partial_len: None,
        active_channels_mask: None,
    };
    {
        let mut destination = InterleavedSlice::new_mut(&mut output, 1, capacity)?;
        let max_steps = samples.len().div_ceil(resampler.input_frames_next().max(1))
            + (((delay + 2 * resampler.output_frames_max()) as f64 * f64::from(input_rate)
                / f64::from(output_rate))
            .ceil() as usize)
                .div_ceil(resampler.input_frames_next().max(1))
            + 8;
        let mut steps = 0;
        while indexing.input_offset < samples.len() || indexing.output_offset < expected + delay {
            ensure!(!cancel.is_cancelled(), "Audio conversion cancelled");
            let available = samples.len().saturating_sub(indexing.input_offset);
            let required = resampler.input_frames_next();
            indexing.partial_len = (available < required).then_some(available);
            let (consumed, produced) =
                resampler.process_into_buffer(&input, &mut destination, Some(&indexing))?;
            steps += 1;
            ensure!(
                steps <= max_steps,
                "Audio resampler exceeded its bounded flush budget"
            );
            indexing.input_offset += (consumed).min(available);
            indexing.output_offset += produced;
        }
    }
    ensure!(
        output.len() >= delay + expected,
        "Resampler returned a short output"
    );
    output.copy_within(delay..delay + expected, 0);
    output.truncate(expected);
    Ok(output)
}

pub(super) fn write_mono(
    path: &Path,
    samples: &[f32],
    rate: u32,
    cancel: &CancellationToken,
) -> Result<AudioInfo> {
    ensure!(
        !samples.is_empty() && samples.len() <= MAX_SECONDS as usize * rate as usize,
        "Invalid audio output duration"
    );
    let file = OpenOptions::new().write(true).create_new(true).open(path)?;
    let result = (|| {
        let mut wav = hound::WavWriter::new(
            BufWriter::new(file),
            hound::WavSpec {
                channels: 1,
                sample_rate: rate,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )?;
        for chunk in samples.chunks(4096) {
            ensure!(!cancel.is_cancelled(), "Audio conversion cancelled");
            for &sample in chunk {
                let value = if sample.is_finite() {
                    sample.clamp(-1., 1.)
                } else {
                    0.
                };
                wav.write_sample((value * 32767.).round() as i16)?;
            }
        }
        wav.finalize()?;
        Ok(AudioInfo {
            path: path.to_path_buf(),
            sample_rate: rate,
            channels: 1,
            frames: samples.len() as u64,
            duration_seconds: samples.len() as f64 / f64::from(rate),
        })
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(path);
    }
    result
}

#[cfg(test)]
mod tests;
