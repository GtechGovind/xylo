//! Downloads page. The helper owns verified release metadata, file I/O,
//! extraction and activation. Merely opening this page never downloads an asset.
//! Progress polling backs off after a failed status read instead of stopping.
use super::settings_ui;
use super::*;
use crate::motion;
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
enum CatalogState {
    #[default]
    Idle,
    Loading,
    Ready,
    Failed(String),
}
#[derive(Default)]
pub(super) struct AssetsUi {
    catalog: Vec<AssetInfo>,
    jobs: Vec<InstallJob>,
    pub filter: usize,
    state: CatalogState,
    poll: Option<Task<()>>,
    /// Consecutive failed status reads while a download is active.
    poll_failures: u32,
    notice: String,
    /// When the pending catalog read was sent, for the slow-wait caption.
    loading_since: Option<std::time::Instant>,
    /// The asset whose Install or Use request awaits the helper's answer.
    pending_action: Option<String>,
}
/// Delay before the next status read: 800 ms, doubling per failure, at most 30 s.
fn poll_delay(failures: u32) -> std::time::Duration {
    std::time::Duration::from_millis((800u64 << failures.min(6)).min(30_000))
}
impl Xylo {
    pub(super) fn open_assets(&mut self, filter: usize, cx: &mut Context<Self>) {
        self.assets.filter = filter;
        self.navigate(Page::Assets, cx);
    }
    pub(super) fn refresh_assets(&mut self, cx: &mut Context<Self>) {
        self.assets.state = CatalogState::Loading;
        self.assets.loading_since = Some(self.arm_slow_hint(cx));
        self.assets.notice.clear();
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
    pub(super) fn asset_error(&mut self, name: &str, error: &str, cx: &mut Context<Self>) -> bool {
        match name {
            "helper_asset_catalog" => {
                self.assets.loading_since = None;
                self.assets.state = CatalogState::Failed(product_error(error))
            }
            // The shared error presentation reports these; only the busy
            // state is settled here.
            "helper_asset_install" | "helper_asset_activate" => {
                self.assets.pending_action = None;
                cx.notify();
                return false;
            }
            "helper_asset_status" => {
                self.assets.poll = None;
                if self.assets.jobs.iter().any(InstallJob::active) {
                    self.assets.poll_failures = self.assets.poll_failures.saturating_add(1);
                    self.assets.notice = format!(
                        "Could not check download progress. {} Retrying…",
                        product_error(error)
                    );
                    self.schedule_asset_status(cx);
                } else {
                    self.assets.notice = format!(
                        "Could not check download progress. {} Refresh to try again.",
                        product_error(error)
                    );
                }
            }
            _ => return false,
        }
        cx.notify();
        true
    }
    pub(super) fn asset_result(
        &mut self,
        name: &str,
        value: &Value,
        cx: &mut Context<Self>,
    ) -> bool {
        match name {
            "helper_asset_catalog" => {
                self.assets.loading_since = None;
                match serde_json::from_value(value.get("assets").cloned().unwrap_or(json!([]))) {
                    Ok(assets) => {
                        self.assets.catalog = assets;
                        self.assets.state = CatalogState::Ready;
                    }
                    Err(_) => {
                        self.assets.state = CatalogState::Failed(
                            "Could not read the list of downloads. Please try again.".into(),
                        );
                    }
                }
            }
            "helper_asset_status" => {
                let was_active = self.assets.jobs.iter().any(InstallJob::active);
                if self.assets.poll_failures > 0 {
                    self.assets.poll_failures = 0;
                    self.assets.notice.clear();
                }
                match serde_json::from_value(value.get("jobs").cloned().unwrap_or(json!([]))) {
                    Ok(jobs) => self.assets.jobs = jobs,
                    Err(_) => {
                        self.assets.notice =
                            "Could not read download progress. Refresh to try again.".into()
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
                self.assets.pending_action = None;
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
                self.assets.pending_action = None;
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
        let delay = poll_delay(self.assets.poll_failures);
        self.assets.poll = Some(cx.spawn(async move |view, cx| {
            cx.background_executor().timer(delay).await;
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
        let filters = settings_ui::segmented(
            "asset-filter",
            ["All", "Models", "Documents", "Voice", "Memory"]
                .into_iter()
                .enumerate()
                .map(|(index, label)| {
                    let view = cx.entity().downgrade();
                    settings_ui::SelectOption::new(
                        label,
                        self.assets.filter == index,
                        move |_, cx| {
                            let _ = view.update(cx, |view, cx| {
                                view.assets.filter = index;
                                cx.notify();
                            });
                        },
                    )
                })
                .collect(),
        );
        let mut page = settings_ui::settings_page("downloads-scroll", "Downloads")
            .description("Features and local models Xylo can run on this computer. Nothing downloads until you choose Install.")
            .action(
                settings_ui::busy(
                    settings_ui::secondary_button("refresh-assets", "Refresh")
                        .on_click(cx.listener(|view, _, _, cx| view.refresh_assets(cx))),
                    matches!(self.assets.state, CatalogState::Loading),
                ),
            );
        if !self.assets.notice.is_empty() {
            page = page.child(notice(BadgeTone::Neutral, self.assets.notice.clone()));
        }
        let body: AnyElement = match &self.assets.state {
            CatalogState::Idle => settings_ui::empty_state(
                AppIcon::Storage,
                "Downloads not loaded",
                "Load the list of features you can add to this computer.",
                Some(
                    settings_ui::primary_button("load-assets", "Load downloads")
                        .on_click(cx.listener(|view, _, _, cx| view.refresh_assets(cx)))
                        .into_any_element(),
                ),
            )
            .into_any_element(),
            CatalogState::Loading => {
                settings_ui::loading_list(3, self.assets.loading_since).into_any_element()
            }
            CatalogState::Failed(error) => div()
                .w_full()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(settings_ui::inline_error(error)),
                )
                .child(settings_ui::busy(
                    settings_ui::secondary_button("retry-assets", "Try again")
                        .on_click(cx.listener(|view, _, _, cx| view.refresh_assets(cx))),
                    matches!(self.assets.state, CatalogState::Loading),
                ))
                .into_any_element(),
            CatalogState::Ready => self.asset_list(cx),
        };
        page.child(settings_ui::section(
            "Available features",
            None,
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap(px(12.))
                .child(filters)
                .child(body),
        ))
        .into_any_element()
    }
    /// The catalog under the current filter as list items: status, progress
    /// or the Install / Use action, and the license in the overflow menu.
    fn asset_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut items: Vec<AnyElement> = Vec::new();
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
            let failed = job.filter(|job| job.phase == "failed");
            let name = if asset.kind == "language-server" {
                "Rust code assistance".to_string()
            } else {
                asset.name.clone()
            };
            let subtitle = if let Some(job) = active_job {
                format!(
                    "{} · {} of {}",
                    phase_label(&job.phase),
                    human_bytes(job.downloaded_bytes),
                    human_bytes(job.total_bytes)
                )
            } else if let Some(job) = failed {
                format!("Download could not finish. {}", product_error(&job.message))
            } else {
                asset_description(&asset.kind).to_owned()
            };
            let mut tags: Vec<SharedString> = vec![human_bytes(asset.size_bytes).into()];
            if !asset.license.identifier.is_empty() {
                tags.push(asset.license.identifier.clone().into());
            }
            if !asset.requires.is_empty() {
                tags.push("Components included".into());
            }
            let mut item = settings_ui::list_item(
                ("asset", i),
                Some(settings_ui::monogram(&name, &asset.id).into_any_element()),
                name,
                Some(subtitle.into()),
            )
            .tags(tags);
            if !asset.supported {
                item = item.trailing(settings_ui::pill(
                    "Not supported",
                    settings_ui::Tone::Neutral,
                ));
            } else if let Some(job) = active_job {
                let job_id = job.job_id.clone();
                item = item
                    .trailing(
                        div()
                            .w(px(72.))
                            .child(motion::progress_bar(("asset-progress", i), job.progress)),
                    )
                    .trailing(
                        settings_ui::text_button(("cancel-download", i), "Cancel").on_click(
                            cx.listener(move |view, _, _, cx| {
                                view.capability(
                                    "helper_asset_cancel",
                                    json!({"job_id":job_id}),
                                    "Cancelling download",
                                    None,
                                    cx,
                                );
                            }),
                        ),
                    );
            } else {
                if asset.active {
                    item = item.trailing(settings_ui::pill("In use", settings_ui::Tone::Success));
                } else if failed.is_some() {
                    item = item.trailing(settings_ui::status_pill(settings_ui::Status::Error));
                } else if installed {
                    item =
                        item.trailing(settings_ui::pill("Installed", settings_ui::Tone::Neutral));
                }
                if !asset.active {
                    let id = asset.id.clone();
                    let pending = self.assets.pending_action.as_deref() == Some(id.as_str());
                    item = item.trailing(
                        settings_ui::secondary_button(
                            ("install-asset", i),
                            if installed { "Use" } else { "Install" },
                        )
                        .on_click(cx.listener(move |view, _, _, cx| {
                            view.assets.pending_action = Some(id.clone());
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
                        }))
                        .map(|button| settings_ui::busy(button, pending)),
                    );
                }
            }
            if !asset.license.url.is_empty() {
                let url = asset.license.url.clone();
                item = item.trailing(settings_ui::overflow_menu(
                    ("asset-actions", i),
                    "More actions",
                    vec![settings_ui::MenuAction::new(
                        "View license",
                        move |_, cx| cx.open_url(&url),
                    )],
                ));
            }
            items.push(item.into_any_element());
        }
        if items.is_empty() {
            return settings_ui::empty_state(
                AppIcon::Storage,
                "Nothing in this category",
                "Choose another category to see more downloads.",
                None,
            )
            .into_any_element();
        }
        settings_ui::list_container(items).into_any_element()
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

#[cfg(test)]
#[path = "assets/tests.rs"]
mod tests;
