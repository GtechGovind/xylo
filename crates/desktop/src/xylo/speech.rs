//! User-driven native speech controls. This module owns request correlation and
//! retains a recording indicator outside the Voice card through the chat button.
use super::*;
use serde_json::{Value, json};

pub(super) struct SpeechUi {
    pub text: Entity<TextareaState>,
    pub transcript: String,
    pub recording_state: String,
    recording_id: Option<Uuid>,
    pending: BTreeMap<Uuid, String>,
    ignored: BTreeSet<Uuid>,
    latest_transcription: Option<Uuid>,
    latest_synthesis: Option<Uuid>,
    poll: Option<Task<()>>,
    elapsed_ms: u64,
    peak: f32,
    pub audio: Option<PathBuf>,
    pub recording: Option<PathBuf>,
    last_transcribed: Option<PathBuf>,
    start_request: Option<Uuid>,
    playback_request: Option<Uuid>,
    pub playing: bool,
    pub notice: String,
}
impl SpeechUi {
    pub(super) fn new(window: &mut Window, cx: &mut Context<Xylo>) -> Self {
        Self {
            text: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .placeholder("Text to read aloud with your selected voice")
                    .rows(3)
            }),
            transcript: String::new(),
            recording_state: "idle".into(),
            recording_id: None,
            pending: BTreeMap::new(),
            ignored: BTreeSet::new(),
            latest_transcription: None,
            latest_synthesis: None,
            poll: None,
            elapsed_ms: 0,
            peak: 0.,
            audio: None,
            recording: None,
            last_transcribed: None,
            start_request: None,
            playback_request: None,
            playing: false,
            notice: String::new(),
        }
    }
    fn capturing(&self) -> bool {
        matches!(
            self.recording_state.as_str(),
            "starting" | "recording" | "finalizing" | "cancelling"
        )
    }
    fn processing(&self) -> bool {
        self.pending
            .values()
            .any(|name| matches!(name.as_str(), "speech_transcribe" | "speech_synthesize"))
    }
    pub(super) fn busy(&self) -> bool {
        self.capturing() || self.playing || self.processing()
    }
}
impl Xylo {
    pub(super) fn speech_recording_active(&self) -> bool {
        self.speech.capturing()
    }
    pub(super) fn global_recording_controls(&self, cx: &mut Context<Self>) -> AnyElement {
        let finishing = matches!(
            self.speech.recording_state.as_str(),
            "finalizing" | "cancelling"
        );
        let label = match self.speech.recording_state.as_str() {
            "starting" => "Waiting for microphone…".into(),
            "finalizing" => "Finishing recording…".into(),
            "cancelling" => "Discarding recording…".into(),
            _ => format!(
                "Recording {:02}:{:02}",
                self.speech.elapsed_ms / 60000,
                (self.speech.elapsed_ms / 1000) % 60
            ),
        };
        div()
            .px_5()
            .py_1()
            .flex()
            .items_center()
            .gap_3()
            .border_b_1()
            .border_color(rgb(border()))
            .child(subtitle(label))
            .child(
                button(
                    "global-recording-stop",
                    if self.speech.recording_state == "starting" {
                        "Cancel"
                    } else {
                        "Stop recording"
                    },
                )
                .disabled(finishing)
                .on_click(cx.listener(|view, _, _, cx| view.speech_record_stop(cx))),
            )
            .into_any_element()
    }

    fn speech_request(&mut self, name: &str, args: Value, cx: &mut Context<Self>) -> Option<Uuid> {
        while self.speech.ignored.len() > 128 {
            self.speech.ignored.pop_first();
        }
        let id = Uuid::new_v4();
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.capability(id, name.into(), args));
        if result.is_ok() {
            self.speech.pending.insert(id, name.into());
            self.error = None;
            self.result(Ok(()), cx);
            Some(id)
        } else {
            self.result(result, cx);
            None
        }
    }
    fn speech_record_start(&mut self, cx: &mut Context<Self>) {
        if self.speech.capturing() || self.speech.playing {
            return;
        }
        self.speech.recording_state = "starting".into();
        self.speech.recording_id = None;
        self.speech.elapsed_ms = 0;
        self.speech.peak = 0.;
        self.speech.notice = "Waiting for microphone permission…".into();
        self.speech.start_request = self.speech_request(
            "helper_audio_record_start",
            json!({"user_initiated":true,"max_seconds":120}),
            cx,
        );
        if self.speech.start_request.is_none() {
            self.speech.recording_state = "idle".into();
        }
        cx.notify();
    }
    fn speech_record_stop(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.speech.recording_id {
            self.speech.recording_state = "finalizing".into();
            self.speech.notice = "Finishing recording…".into();
            self.speech_request("helper_audio_record_stop", json!({"recording_id":id}), cx);
        } else {
            self.speech_cancel_recording(cx);
        }
        cx.notify();
    }
    fn speech_cancel_recording(&mut self, cx: &mut Context<Self>) {
        let stale: Vec<_> = self
            .speech
            .pending
            .iter()
            .filter(|(_, name)| {
                matches!(
                    name.as_str(),
                    "helper_audio_record_start" | "helper_audio_record_status"
                )
            })
            .map(|(id, _)| *id)
            .collect();
        for id in stale {
            self.speech.pending.remove(&id);
            self.speech.ignored.insert(id);
        }
        if let Some(id) = self.speech.start_request.take()
            && let Some(host) = self.host.as_ref()
        {
            let _ = host.cancel_capability(id);
        }
        self.speech.poll = None;
        self.speech.recording_state = "cancelling".into();
        self.speech.notice = "Discarding recording…".into();
        let args = self
            .speech
            .recording_id
            .map(|id| json!({"recording_id":id}))
            .unwrap_or(json!({}));
        self.speech_request("helper_audio_record_cancel", args, cx);
        cx.notify();
    }
    fn speech_schedule_status(&mut self, cx: &mut Context<Self>) {
        if self.quitting || !self.speech.capturing() {
            self.speech.poll = None;
            return;
        }
        self.speech.poll = Some(cx.spawn(async move |view, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(400))
                .await;
            let _ = view.update(cx, |view, cx| {
                if view.speech.capturing() {
                    view.speech_request("helper_audio_record_status", json!({}), cx);
                }
            });
        }));
    }
    pub(super) fn speech_transcribe_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.speech.recording = Some(path.clone());
        self.speech.last_transcribed = Some(path.clone());
        self.speech.notice = "Transcribing on this computer…".into();
        self.speech.latest_transcription = self.speech_request(
            "speech_transcribe",
            json!({"engine":"whisper","path":path}),
            cx,
        );
        cx.notify();
    }
    pub(super) fn speech_speak_text(&mut self, text: String, cx: &mut Context<Self>) {
        if text.trim().is_empty() {
            self.error = Some("Enter text to create speech.".into());
            cx.notify();
            return;
        }
        self.speech.notice = "Creating speech on this computer…".into();
        self.speech.latest_synthesis = self.speech_request(
            "speech_synthesize",
            json!({"engine":"sherpa","text":text}),
            cx,
        );
        cx.notify();
    }
    fn speech_play(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.speech.playing || self.speech.capturing() {
            return;
        }
        self.speech.playing = true;
        self.speech.notice = "Playing audio…".into();
        self.speech.playback_request = self.speech_request(
            "helper_audio_play",
            json!({"path":path,"user_initiated":true,"volume":0.8}),
            cx,
        );
        if self.speech.playback_request.is_none() {
            self.speech.playing = false;
        }
        cx.notify();
    }
    fn speech_stop_playback(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.speech.playback_request.take()
            && let Some(host) = self.host.as_ref()
        {
            let _ = host.cancel_capability(id);
        }
        self.speech_request("helper_audio_play_stop", json!({}), cx);
        self.speech.playing = false;
        self.speech.notice = "Playback stopped.".into();
        cx.notify();
    }
    /// Call before changing workspace and before host shutdown. Cancelling both
    /// the startup request and current recording closes response-order races.
    pub(super) fn stop_speech(&mut self, cx: &mut Context<Self>) {
        self.speech.poll = None;
        if let Some(host) = self.host.as_ref() {
            for id in self.speech.pending.keys() {
                let _ = host.cancel_capability(*id);
            }
        }
        self.speech
            .ignored
            .extend(self.speech.pending.keys().copied());
        self.speech.pending.clear();
        if self.speech.capturing() || self.speech.start_request.is_some() {
            self.speech_cancel_recording(cx);
        }
        if self.speech.playing {
            self.speech_stop_playback(cx);
        }
    }
    pub(super) fn speech_result(
        &mut self,
        id: Uuid,
        name: &str,
        value: &Value,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.speech.ignored.remove(&id) {
            return true;
        }
        if self.speech.pending.remove(&id).is_none() {
            return false;
        }
        if (name == "speech_transcribe" && self.speech.latest_transcription != Some(id))
            || (name == "speech_synthesize" && self.speech.latest_synthesis != Some(id))
        {
            return true;
        }
        match name {
            "helper_audio_record_start"
            | "helper_audio_record_status"
            | "helper_audio_record_stop" => {
                self.speech.start_request = None;
                if let Some(id) = value
                    .get("recording_id")
                    .and_then(Value::as_str)
                    .and_then(|id| Uuid::parse_str(id).ok())
                {
                    self.speech.recording_id = Some(id);
                }
                self.speech.recording_state = value
                    .get("state")
                    .and_then(Value::as_str)
                    .unwrap_or("idle")
                    .into();
                self.speech.elapsed_ms =
                    value.get("elapsed_ms").and_then(Value::as_u64).unwrap_or(0);
                self.speech.peak = value
                    .get("peak")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.)
                    .clamp(0., 1.) as f32;
                if self.speech.capturing() {
                    self.speech.notice = "Recording locally. Stop to transcribe.".into();
                    self.speech_schedule_status(cx);
                } else {
                    self.speech.poll = None;
                    if self.speech.recording_state == "recorded"
                        && let Some(path) =
                            value.get("path").and_then(Value::as_str).map(PathBuf::from)
                    {
                        self.speech.recording = Some(path.clone());
                        self.record_artifact(path.clone());
                        if self.speech.last_transcribed.as_ref() != Some(&path) {
                            self.speech_transcribe_path(path, cx);
                        }
                    } else if self.speech.recording_state == "failed" {
                        self.error = Some(
                            value
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("Recording failed; check microphone privacy settings.")
                                .into(),
                        );
                    }
                }
            }
            "helper_audio_record_cancel" => {
                self.speech.poll = None;
                self.speech.recording_state = "idle".into();
                self.speech.recording_id = None;
                self.speech.notice = "Recording discarded.".into();
            }
            "speech_transcribe" => {
                self.speech.transcript = value
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .into();
                self.speech.notice = if self.speech.transcript.is_empty() {
                    "No speech was recognized. You can retry the recording.".into()
                } else {
                    "Transcript ready. Review it before adding it to your message.".into()
                };
            }
            "speech_synthesize" => {
                if let Some(path) = value.get("path").and_then(Value::as_str).map(PathBuf::from) {
                    self.speech.audio = Some(path.clone());
                    self.record_artifact(path);
                }
                self.speech.notice = "Audio ready. Choose Play to listen.".into();
            }
            "helper_audio_play" | "helper_audio_play_stop" => {
                self.speech.playing = false;
                self.speech.playback_request = None;
                self.speech.notice = "Playback finished.".into();
            }
            _ => {}
        }
        cx.notify();
        true
    }
    pub(super) fn speech_error(
        &mut self,
        id: Uuid,
        name: &str,
        message: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.speech.ignored.remove(&id) {
            return true;
        }
        if self.speech.pending.remove(&id).is_none() {
            return false;
        }
        if (name == "speech_transcribe" && self.speech.latest_transcription != Some(id))
            || (name == "speech_synthesize" && self.speech.latest_synthesis != Some(id))
        {
            return true;
        }
        if name.starts_with("helper_audio_record_") {
            self.speech.poll = None;
            self.speech.recording_state = "failed".into();
            self.speech.start_request = None;
        }
        if name.starts_with("helper_audio_play") {
            self.speech.playing = false;
            self.speech.playback_request = None;
        }
        if message.to_ascii_lowercase().contains("cancel") {
            self.speech.notice = "Speech task cancelled.".into();
            self.error = None;
        } else {
            self.speech.notice = "Speech could not finish.".into();
            self.error = Some(message.into());
        }
        cx.notify();
        true
    }
    pub(super) fn speech_capture_button(&self, cx: &mut Context<Self>) -> AnyElement {
        let active = self.speech.capturing();
        let label = if self.speech.recording_state == "starting" {
            "Cancel microphone"
        } else if active {
            "Stop recording"
        } else {
            "Record voice"
        };
        icon_button(
            "record-voice-composer",
            label,
            if active {
                gpui_kit::component::IconName::Square
            } else {
                gpui_kit::component::IconName::Mic
            },
        )
        .on_click(cx.listener(move |view, _, _, cx| {
            if active {
                view.speech_record_stop(cx);
            } else {
                view.speech_record_start(cx);
            }
        }))
        .into_any_element()
    }
    pub(super) fn speech_card(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut panel=card().child(div().font_weight(FontWeight::MEDIUM).child("Voice"))
            .child(subtitle("Record up to two minutes. Audio stays on this computer and is transcribed with your selected model."))
            .child(button("speech-download-models","Browse voices and transcription").on_click(cx.listener(|view,_,_,cx|view.open_assets(3,cx))))
            .child(div().flex().gap_2().child(self.speech_capture_button(cx)).when(self.speech.capturing(),|row|row.child(button("speech-discard","Discard recording").on_click(cx.listener(|view,_,_,cx|view.speech_cancel_recording(cx)))))
                .child(button("speech-open-audio","Transcribe project audio").on_click(cx.listener(|view,_,window,cx|view.speech_pick_audio(window,cx)))));
        if self.speech.capturing() {
            panel = panel.child(subtitle(format!(
                "Recording {:02}:{:02} · Input level {:.0}%",
                self.speech.elapsed_ms / 60000,
                self.speech.elapsed_ms / 1000 % 60,
                self.speech.peak * 100.
            )));
        }
        if !self.speech.transcript.is_empty() {
            panel = panel
                .child(
                    div()
                        .text_size(px(13.))
                        .child(self.speech.transcript.clone()),
                )
                .child(
                    button("speech-use-transcript", "Add transcript to message").on_click(
                        cx.listener(|view, _, window, cx| {
                            let current = view.composer.read(cx).value().to_string();
                            let text = if current.trim().is_empty() {
                                view.speech.transcript.clone()
                            } else {
                                format!("{current}\n{}", view.speech.transcript)
                            };
                            view.composer
                                .update(cx, |state, cx| state.set_value(text, window, cx));
                            view.navigate(Page::Chat, cx);
                        }),
                    ),
                );
        }
        if let Some(path) = self.speech.recording.as_ref() {
            let retry = path.clone();
            let listen = path.clone();
            panel = panel.child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        button("speech-retry-transcription", "Retry transcription").on_click(
                            cx.listener(move |view, _, _, cx| {
                                view.speech_transcribe_path(retry.clone(), cx)
                            }),
                        ),
                    )
                    .child(button("speech-play-recording", "Play recording").on_click(
                        cx.listener(move |view, _, _, cx| view.speech_play(listen.clone(), cx)),
                    )),
            );
        }
        panel = panel.child(Textarea::new(&self.speech.text)).child(
            primary("speech-create-native", "Create speech").on_click(cx.listener(
                |view, _, _, cx| {
                    let text = view.speech.text.read(cx).value().to_string();
                    view.speech_speak_text(text, cx);
                },
            )),
        );
        if let Some(path) = self.speech.audio.as_ref() {
            let path = path.clone();
            panel = panel.child(
                button("speech-play-generated", "Play generated audio").on_click(
                    cx.listener(move |view, _, _, cx| view.speech_play(path.clone(), cx)),
                ),
            );
        }
        if self.speech.playing {
            panel = panel.child(
                button("speech-stop-playback", "Stop playback")
                    .on_click(cx.listener(|view, _, _, cx| view.speech_stop_playback(cx))),
            );
        }
        if self.speech.processing() {
            panel = panel.child(
                button("speech-cancel-processing", "Cancel speech task").on_click(cx.listener(
                    |view, _, _, _cx| {
                        if let Some(host) = view.host.as_ref() {
                            for (id, name) in &view.speech.pending {
                                if matches!(
                                    name.as_str(),
                                    "speech_transcribe" | "speech_synthesize"
                                ) {
                                    let _ = host.cancel_capability(*id);
                                }
                            }
                        }
                    },
                )),
            );
        }
        if !self.speech.notice.is_empty() {
            panel = panel.child(subtitle(self.speech.notice.clone()));
        }
        panel.into_any_element()
    }
    fn speech_pick_audio(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |view, cx| {
            let mut picker = rfd::AsyncFileDialog::new()
                .set_title("Choose project audio")
                .add_filter("Audio", &["wav", "mp3", "m4a", "flac", "ogg"]);
            if let Some(workspace) = workspace {
                picker = picker.set_directory(workspace);
            }
            if let Some(file) = picker.pick_file().await {
                let path = file.path().to_path_buf();
                let _ = view.update_in(cx, |view, _, cx| view.speech_transcribe_path(path, cx));
            }
        })
        .detach();
    }
}

#[cfg(test)]
#[allow(dead_code)] // Used by the standalone native validation test target.
impl SpeechUi {
    /// Seed a backend request without a device, process or model operation.
    pub(super) fn validation_request(&mut self, id: Uuid, name: &str) {
        self.pending.insert(id, name.to_owned());
        match name {
            "speech_transcribe" => self.latest_transcription = Some(id),
            "speech_synthesize" => self.latest_synthesis = Some(id),
            "helper_audio_record_start" => {
                self.start_request = Some(id);
                self.recording_state = "starting".into();
            }
            _ => {}
        }
    }
}
