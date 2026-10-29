//! Product installation UI. The helper owns verified release metadata, file I/O,
//! extraction and activation. Merely opening this page never downloads an asset.
use super::*;
use serde::Deserialize;
use serde_json::{Value, json};
#[derive(Clone, Default, Deserialize)]
#[serde(default)]
struct License {
    identifier: String,
    url: String,
}
#[derive(Clone, Default, Deserialize)]
#[serde(default)]
struct AssetInfo {
    id: String,
    name: String,
    kind: String,
    size_bytes: u64,
    license: License,
    supported: bool,
    hidden: bool,
    installed: bool,
    active: bool,
    requires: Vec<String>,
}
#[derive(Clone, Default, Deserialize)]
#[serde(default)]
struct InstallJob {
    job_id: String,
    asset_id: String,
    phase: String,
    downloaded_bytes: u64,
    total_bytes: u64,
    progress: f32,
    message: String,
}
impl InstallJob {
    fn active(&self) -> bool {
        !matches!(
            self.phase.as_str(),
            "installed"
                | "completed"
                | "failed"
                | "cancelled"
                | "interrupted"
                | "ready"
                | "succeeded"
        )
    }
}
#[derive(Default)]
pub(super) struct AssetsUi {
    catalog: Vec<AssetInfo>,
    jobs: Vec<InstallJob>,
    pub filter: usize,
    loaded: bool,
    poll: Option<Task<()>>,
    notice: String,
}
impl Xylo {
    pub(super) fn open_assets(&mut self, filter: usize, cx: &mut Context<Self>) {
        self.assets.filter = filter;
        self.navigate(Page::Assets, cx);
        self.capability(
            "helper_asset_catalog",
            json!({}),
            "Loading available features",
            None,
            cx,
        );
        self.capability(
            "helper_asset_status",
            json!({}),
            "Checking downloads",
            None,
            cx,
        );
    }
    pub(super) fn asset_result(
        &mut self,
        name: &str,
        value: &Value,
        cx: &mut Context<Self>,
    ) -> bool {
        match name {
            "helper_asset_catalog" => {
                match serde_json::from_value(value.get("assets").cloned().unwrap_or(json!([]))) {
                    Ok(assets) => {
                        self.assets.catalog = assets;
                        self.assets.loaded = true;
                    }
                    Err(error) => {
                        self.error = Some(format!("Could not read available features: {error}"));
                    }
                }
            }
            "helper_asset_status" => {
                let was_active = self.assets.jobs.iter().any(InstallJob::active);
                match serde_json::from_value(value.get("jobs").cloned().unwrap_or(json!([]))) {
                    Ok(jobs) => self.assets.jobs = jobs,
                    Err(error) => {
                        self.error = Some(format!("Could not read download progress: {error}"))
                    }
                }
                if self.assets.jobs.iter().any(InstallJob::active) {
                    self.schedule_asset_status(cx);
                } else {
                    self.assets.poll = None;
                    if was_active {
                        self.capability(
                            "helper_asset_catalog",
                            json!({}),
                            "Refreshing available features",
                            None,
                            cx,
                        );
                    }
                }
            }
            "helper_asset_install" => {
                if let Ok(job) = serde_json::from_value::<InstallJob>(value.clone()) {
                    self.assets
                        .jobs
                        .retain(|previous| previous.job_id != job.job_id);
                    self.assets.jobs.push(job);
                }
                self.assets.notice = "Your download has started.".into();
                self.capability(
                    "helper_asset_status",
                    json!({}),
                    "Checking downloads",
                    None,
                    cx,
                );
            }
            "helper_asset_cancel" => {
                self.assets.notice = "Download cancellation requested.".into();
                self.capability(
                    "helper_asset_status",
                    json!({}),
                    "Checking downloads",
                    None,
                    cx,
                );
            }
            "helper_asset_activate" => {
                self.assets.notice = if value
                    .get("restart_required")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    "Ready to use. Restart Xylo to finish applying this feature.".into()
                } else {
                    "Ready to use.".into()
                };
                self.capability(
                    "helper_asset_catalog",
                    json!({}),
                    "Refreshing available features",
                    None,
                    cx,
                );
                self.capability("helper_get_config", json!({}), "Refreshing setup", None, cx);
            }
            _ => return false,
        }
        cx.notify();
        true
    }
    pub(super) fn stop_asset_poll(&mut self) {
        self.assets.poll = None;
    }
    fn schedule_asset_status(&mut self, cx: &mut Context<Self>) {
        if self.quitting {
            return;
        }
        self.assets.poll = Some(cx.spawn(async move |view, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(800))
                .await;
            let _ = view.update(cx, |view, cx| {
                view.capability(
                    "helper_asset_status",
                    json!({}),
                    "Checking downloads",
                    None,
                    cx,
                )
            });
        }));
    }
    pub(super) fn assets_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut filters = div().flex().gap_2();
        for (i, label) in ["All", "Models", "Documents", "Voice", "Memory"]
            .into_iter()
            .enumerate()
        {
            filters = filters.child(
                button(("asset-filter", i), label)
                    .bg(rgb(if self.assets.filter == i {
                        selected()
                    } else {
                        panel()
                    }))
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.assets.filter = i;
                        cx.notify();
                    })),
            );
        }
        let mut page = div()
            .id("downloads-scroll")
            .size_full()
            .overflow_y_scroll()
            .p_6()
            .flex()
            .flex_col()
            .gap_4()
            .child(heading(
                "Downloads",
                "Choose the features and local models you want to use.",
            ))
            .child(filters)
            .child(
                button("refresh-assets", "Refresh").on_click(cx.listener(|view, _, _, cx| {
                    view.capability(
                        "helper_asset_catalog",
                        json!({}),
                        "Refreshing available features",
                        None,
                        cx,
                    );
                    view.capability(
                        "helper_asset_status",
                        json!({}),
                        "Checking downloads",
                        None,
                        cx,
                    );
                })),
            );
        if !self.assets.notice.is_empty() {
            page = page.child(subtitle(self.assets.notice.clone()));
        }
        if !self.assets.loaded {
            page = page.child(subtitle("Loading available features…"));
        }
        for (i, asset) in self
            .assets
            .catalog
            .iter()
            .enumerate()
            .filter(|(_, asset)| {
                !asset.hidden && !matches!(asset.id.as_str(), "llama-runtime" | "whisper-runtime")
            })
            .filter(|(_, asset)| match self.assets.filter {
                1 => matches!(asset.kind.as_str(), "model" | "llama"),
                2 => asset.kind == "pdf",
                3 => matches!(asset.kind.as_str(), "voice" | "whisper" | "runtime"),
                4 => asset.kind == "embedding",
                _ => true,
            })
        {
            let job = self
                .assets
                .jobs
                .iter()
                .rev()
                .find(|job| job.asset_id == asset.id);
            let installed = asset.installed
                || job.is_some_and(|job| {
                    matches!(
                        job.phase.as_str(),
                        "installed" | "completed" | "ready" | "succeeded"
                    )
                });
            let active_job = job.filter(|job| job.active());
            let mut row = card()
                .max_w(px(880.))
                .child(
                    div()
                        .flex()
                        .justify_between()
                        .items_center()
                        .child(div().font_weight(FontWeight::SEMIBOLD).child(
                            if asset.kind == "language-server" {
                                "Rust code assistance".to_string()
                            } else {
                                asset.name.clone()
                            },
                        ))
                        .child(subtitle(if asset.active {
                            "In use"
                        } else if installed {
                            "Installed"
                        } else {
                            "Available"
                        })),
                )
                .child(div().text_sm().child(asset_description(&asset.kind)))
                .child(subtitle(format!(
                    "{} · {}",
                    human_bytes(asset.size_bytes),
                    asset.license.identifier
                )));
            if !asset.requires.is_empty() {
                row = row.child(subtitle(
                    "Required components are included in this download.",
                ));
            }
            if !asset.supported {
                row = row.child(subtitle("Not available for this computer."));
            } else if let Some(job) = active_job {
                let job_id = job.job_id.clone();
                row = row
                    .child(subtitle(format!(
                        "{} · {} / {}",
                        phase_label(&job.phase),
                        human_bytes(job.downloaded_bytes),
                        human_bytes(job.total_bytes)
                    )))
                    .child(
                        div()
                            .w_full()
                            .h(px(5.))
                            .rounded_full()
                            .bg(rgb(border()))
                            .child(
                                div()
                                    .w(relative(job.progress.clamp(0., 1.)))
                                    .h_full()
                                    .bg(rgb(accent()))
                                    .rounded_full(),
                            ),
                    )
                    .child(button(("cancel-download", i), "Cancel download").on_click(
                        cx.listener(move |view, _, _, cx| {
                            view.capability(
                                "helper_asset_cancel",
                                json!({"job_id":job_id}),
                                "Cancelling download",
                                None,
                                cx,
                            );
                        }),
                    ));
            } else if !asset.active {
                let id = asset.id.clone();
                row = row.child(
                    primary(
                        ("install-asset", i),
                        if installed { "Use" } else { "Install" },
                    )
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.capability(
                            if installed {
                                "helper_asset_activate"
                            } else {
                                "helper_asset_install"
                            },
                            json!({"id":id}),
                            if installed {
                                "Applying feature"
                            } else {
                                "Starting download"
                            },
                            None,
                            cx,
                        );
                    })),
                );
            }
            if let Some(job) = job.filter(|job| job.phase == "failed") {
                row = row.child(subtitle(format!(
                    "Download could not finish. {}",
                    product_error(&job.message)
                )));
            }
            if !asset.license.url.is_empty() {
                let url = asset.license.url.clone();
                row = row.child(
                    button(("asset-license", i), "License")
                        .text_xs()
                        .on_click(move |_, _, cx| cx.open_url(&url)),
                );
            }
            page = page.child(row);
        }
        page.into_any_element()
    }
}
fn human_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GB", bytes as f64 / (1024. * 1024. * 1024.))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024. * 1024.))
    } else {
        format!("{} KB", bytes / 1024)
    }
}
fn phase_label(phase: &str) -> &'static str {
    match phase {
        "queued" => "Queued",
        "downloading" => "Downloading",
        "verifying" => "Verifying download",
        "failed" => "Could not finish",
        _ => "Preparing",
    }
}

fn asset_description(kind: &str) -> &'static str {
    match kind {
        "model" => "Run this model on your computer. Choose it after installation.",
        "pdf" => "Open PDF files, extract text, and view page previews.",
        "whisper" => "Turn English audio into text on your computer.",
        "embedding" => "Find saved memories by meaning, including related words.",
        "voice" => "Create spoken English audio from your text.",
        "language-server" => {
            "Get suggestions, explanations, and error highlights while editing Rust files."
        }
        _ => "Add this feature to your workspace.",
    }
}
