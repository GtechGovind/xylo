//! Native speech processing; recording and playback are separate user actions.
use crate::{
    CapabilityConfig, input_path, observation, output_path,
    process::{self, ProcessSpec},
    required_str,
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf, time::Duration};
use tokio_util::sync::CancellationToken;
use workbench_core::ToolObservation;

struct TemporaryAudio(Option<PathBuf>);
impl Drop for TemporaryAudio {
    fn drop(&mut self) {
        if let Some(path) = self.0.as_ref() {
            let _ = std::fs::remove_file(path);
        }
    }
}

pub(crate) async fn execute(
    config: &CapabilityConfig,
    name: &str,
    args: &Value,
    cancel: CancellationToken,
) -> Result<ToolObservation> {
    let engine =
        args.get("engine")
            .and_then(Value::as_str)
            .unwrap_or(if name == "speech_synthesize" {
                "sherpa"
            } else {
                "whisper"
            });
    let helper = match engine {
        "whisper" => config.whisper.as_ref(),
        "sherpa" => config.sherpa.as_ref(),
        _ => anyhow::bail!("Unknown native speech engine"),
    }
    .context(
        "Install and choose a transcription model or voice in Downloads before using speech",
    )?;
    ensure!(
        name != "speech_synthesize" || engine == "sherpa",
        "Speech synthesis requires the selected native sherpa voice"
    );
    let _lease = crate::models::acquire_inference_lock(config, &cancel).await?;
    let mut substitutions = BTreeMap::new();
    for (key, path) in &helper.assets {
        ensure!(
            key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && path.is_absolute()
                && path.is_file(),
            "Speech model asset is missing; reinstall the selected voice/transcription model in Downloads"
        );
        substitutions.insert(key.clone(), path.to_string_lossy().into_owned());
    }
    ensure!(
        !helper.assets.is_empty(),
        "Choose a transcription model or voice in Downloads before using speech"
    );
    let mut normalized = None;
    if name == "speech_transcribe" {
        let input = input_path(config, required_str(args, "path")?)?;
        let destination = output_path(
            config,
            &format!("speech-input-{}", uuid::Uuid::new_v4()),
            "wav",
        )?;
        let token = cancel.clone();
        let output = destination.clone();
        let result = tokio::task::spawn_blocking(move || {
            crate::audio::convert::normalize(
                &input,
                &output,
                crate::audio::convert::MAX_SECONDS,
                &token,
            )
        })
        .await??;
        substitutions.insert("input".into(), result.path.to_string_lossy().into_owned());
        normalized = Some(TemporaryAudio(Some(destination)));
    } else {
        let text = required_str(args, "text")?;
        ensure!(
            !text.trim().is_empty() && text.chars().count() <= 4000,
            "Speech text must contain 1–4000 characters"
        );
        substitutions.insert("text".into(), text.into());
    }
    ensure!(
        helper.output_extension.len() <= 12
            && helper
                .output_extension
                .chars()
                .all(|c| c.is_ascii_alphanumeric()),
        "Invalid speech output extension"
    );
    let output = output_path(
        config,
        &format!("speech-{}", uuid::Uuid::new_v4()),
        &helper.output_extension,
    )?;
    substitutions.insert("output".into(), output.to_string_lossy().into_owned());
    substitutions.insert(
        "output_stem".into(),
        output.with_extension("").to_string_lossy().into_owned(),
    );
    substitutions.insert(
        "threads".into(),
        std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1)
            .saturating_sub(1)
            .max(1)
            .to_string(),
    );
    let template = if name == "speech_synthesize" {
        helper.synthesis_args.as_ref().context(
            "Selected voice has no synthesis adapter; install a supported voice in Downloads",
        )?
    } else {
        &helper.args
    };
    let argv = crate::models::expand_arguments(template, &substitutions)?;
    let mut output_guard = TemporaryAudio(Some(output.clone()));
    let result = process::run(
        ProcessSpec {
            executable: helper.executable.clone(),
            args: argv,
            cwd: config.workspace.clone(),
            env: BTreeMap::new(),
            timeout: Duration::from_secs(config.operation_timeout_seconds),
            output_limit: config.max_output_bytes,
        },
        cancel.clone(),
    )
    .await;
    drop(normalized);
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            let _ = tokio::fs::remove_file(&output).await;
            return Err(error);
        }
    };
    if !result.success {
        let _ = tokio::fs::remove_file(&output).await;
        anyhow::bail!(
            "Native speech failed (exit {:?}): {}",
            result.exit_code,
            result.stderr
        );
    }
    ensure!(
        output.is_file(),
        "Native speech produced no output file; reinstall the matching model/runtime adapter"
    );
    let size = output.metadata()?.len();
    ensure!(
        (name == "speech_transcribe" || size > 0) && size <= crate::audio::convert::MAX_AUDIO_FILE,
        "Native speech output is empty or exceeds its 64MiB budget"
    );
    let audio_info = if name == "speech_synthesize" && helper.output_extension == "wav" {
        let wav = hound::WavReader::open(&output)
            .context("Native voice did not produce a valid WAV file")?;
        let spec = wav.spec();
        ensure!(
            wav.duration() > 0
                && (8000..=192000).contains(&spec.sample_rate)
                && (1..=8).contains(&spec.channels),
            "Native voice returned an unsupported audio layout"
        );
        let duration = f64::from(wav.duration()) / f64::from(spec.sample_rate);
        ensure!(
            duration <= f64::from(crate::audio::convert::MAX_SECONDS),
            "Generated speech exceeds 120 seconds; shorten the text and try again"
        );
        Some(
            json!({"sample_rate":spec.sample_rate,"channels":spec.channels,"duration_seconds":duration}),
        )
    } else {
        None
    };
    let transcript = if name == "speech_transcribe" {
        ensure!(size <= 256 * 1024, "Speech transcript exceeds byte budget");
        let text = tokio::fs::read_to_string(&output).await?;
        Some(text.trim().to_owned())
    } else {
        None
    };
    output_guard.0 = None;
    Ok(observation(
        json!({"engine":engine,"path":output,"text":transcript,"audio":audio_info,"stdout":result.stdout,"truncated":result.truncated,"native_process_completed":true}),
        vec![output],
    ))
}
