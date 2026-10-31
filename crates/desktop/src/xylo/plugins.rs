//! Reviewed plugin packages and explicit grants. Management stays on the native
//! control channel; a run is pinned to one installed package and project.
use super::*;
use serde_json::{Value, json};
#[path = "plugins_view.rs"]
mod view;
#[derive(Clone)]
enum RequestKind {
    List,
    Inspect(PathBuf),
    Mutation {
        operation_id: Uuid,
        inspection: Option<String>,
    },
    Run,
}
struct Request {
    epoch: u64,
    kind: RequestKind,
    tool: String,
    args: Value,
}
#[derive(Clone)]
struct Retry {
    tool: String,
    args: Value,
    kind: RequestKind,
}
pub(super) struct PluginUi {
    catalog: Option<PluginCatalog>,
    retry: Option<Retry>,
    selected: Option<InstalledPlugin>,
    grants: BTreeSet<String>,
    inspection: Option<PluginInspection>,
    manifest: Option<PathBuf>,
    review_grants: BTreeSet<String>,
    enable_install: bool,
    pub input: Entity<TextareaState>,
    output: Entity<TextareaState>,
    artifacts: Vec<PathBuf>,
    pending: BTreeMap<Uuid, Request>,
    retired: BTreeSet<Uuid>,
    epoch: u64,
    pub generation: u64,
    picking: bool,
    confirming: bool,
    pub notice: String,
    /// When the latest request began, for the slow caption.
    pending_since: Option<std::time::Instant>,
}
impl PluginUi {
    pub fn new(window: &mut Window, cx: &mut Context<Xylo>) -> Self {
        Self {
            catalog: None,
            retry: None,
            selected: None,
            grants: BTreeSet::new(),
            inspection: None,
            manifest: None,
            review_grants: BTreeSet::new(),
            enable_install: false,
            input: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .rows(3)
                    .placeholder("Input for this plugin")
            }),
            output: cx.new(|cx| TextareaState::new(window, cx).rows(6)),
            artifacts: Vec::new(),
            pending: BTreeMap::new(),
            retired: BTreeSet::new(),
            epoch: 0,
            generation: 0,
            picking: false,
            confirming: false,
            notice: String::new(),
            pending_since: None,
        }
    }
    /// Whether a request for `tool` is in flight, so only its button spins.
    fn pending_tool(&self, tool: &str) -> bool {
        self.pending.values().any(|request| request.tool == tool)
    }
    pub fn busy(&self) -> bool {
        !self.pending.is_empty() || self.picking || self.confirming
    }
    pub fn active(&self) -> bool {
        self.confirming
            || self.pending.values().any(|request| {
                matches!(
                    request.kind,
                    RequestKind::Mutation { .. } | RequestKind::Run
                )
            })
    }
    pub fn dirty(&self) -> bool {
        self.inspection.is_some() || self.grants_dirty() || self.retry.is_some()
    }
    fn grants_dirty(&self) -> bool {
        self.selected
            .as_ref()
            .is_some_and(|plugin| self.grants != plugin.grants.iter().cloned().collect())
    }
    fn selected_changed(&self) -> bool {
        self.selected.as_ref().is_some_and(|selected| {
            self.catalog.as_ref().is_none_or(|catalog| {
                !catalog.plugins.iter().any(|plugin| {
                    plugin.manifest.id == selected.manifest.id
                        && plugin.package_sha256 == selected.package_sha256
                })
            })
        })
    }
}
impl Xylo {
    fn plugin_request(
        &mut self,
        name: &str,
        args: Value,
        kind: RequestKind,
        cx: &mut Context<Self>,
    ) {
        if self.quitting || !self.plugins.pending.is_empty() {
            return;
        }
        let id = Uuid::new_v4();
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.capability(id, name.into(), args.clone()));
        if result.is_ok() {
            self.plugins.pending_since = Some(self.arm_slow_hint(cx));
            self.plugins.pending.insert(
                id,
                Request {
                    epoch: self.plugins.epoch,
                    kind,
                    tool: name.into(),
                    args,
                },
            );
        } else {
            self.plugins.notice =
                "The request could not start. Your choices remain here; try again.".into();
        }
        self.result(result, cx);
    }
    /// Opening Plugins loads installed packages and native modules, searches
    /// the cached plugin directory and refreshes it in the background.
    pub(super) fn refresh_plugins(&mut self, cx: &mut Context<Self>) {
        self.extension_request("extension_snapshot", json!({}), cx);
        self.extension_directory_open(cx);
        self.plugin_request("helper_plugin_list", json!({}), RequestKind::List, cx);
    }
    pub(super) fn plugin_owns_request(&self, id: Uuid) -> bool {
        self.plugins.pending.contains_key(&id) || self.plugins.retired.contains(&id)
    }
    pub(super) fn plugin_scope_changed(&mut self, cx: &mut Context<Self>) {
        let ids = self
            .plugins
            .pending
            .iter()
            .filter(|(_, request)| {
                matches!(request.kind, RequestKind::List | RequestKind::Inspect(..))
            })
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        for id in ids {
            if let Some(host) = &self.host {
                let _ = host.cancel_capability(id);
            }
            self.plugins.pending.remove(&id);
            self.plugins.retired.insert(id);
        }
        self.plugins.epoch = self.plugins.epoch.saturating_add(1);
        self.plugins.picking = false;
        while self.plugins.retired.len() > 128 {
            self.plugins.retired.pop_first();
        }
        cx.notify();
    }
    fn plugin_pick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.plugins.busy() || self.plugins.retry.is_some() {
            return;
        }
        let epoch = self.plugins.epoch;
        self.plugins.picking = true;
        cx.spawn_in(window, async move |view, cx| {
            let file = rfd::AsyncFileDialog::new()
                .set_title("Choose a plugin package manifest")
                .add_filter("Plugin package", &["json"])
                .pick_file()
                .await;
            let _ = view.update_in(cx, |view, _, cx| {
                if epoch != view.plugins.epoch {
                    return;
                }
                view.plugins.picking = false;
                if let Some(file) = file {
                    view.plugin_inspect(file.path().to_path_buf(), cx);
                }
                cx.notify();
            });
        })
        .detach();
    }
    pub(super) fn plugin_inspect(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.plugins.retry.is_some() || self.plugins.busy() {
            return;
        }
        self.plugins.inspection = None;
        self.plugins.manifest = None;
        self.plugins.review_grants.clear();
        self.plugins.enable_install = false;
        self.plugins.generation = self.plugins.generation.saturating_add(1);
        self.cancel_setup_close_intents(cx);
        self.plugins.notice = "Reviewing the selected package…".into();
        self.plugin_request(
            "helper_plugin_inspect",
            json!({"manifest_path":path}),
            RequestKind::Inspect(path),
            cx,
        );
    }
    fn plugin_select(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.plugins.busy() {
            return;
        }
        if self
            .plugins
            .selected
            .as_ref()
            .is_some_and(|plugin| plugin.manifest.id == id)
            && !self.plugins.selected_changed()
        {
            return;
        }
        if self.plugins.grants_dirty() {
            let epoch = self.plugins.epoch;
            let generation = self.plugins.generation;
            self.plugins.confirming = true;
            let answer = window.prompt(
                PromptLevel::Warning,
                "Discard unsaved permission changes?",
                Some("The plugin's saved permissions will stay unchanged."),
                &[
                    PromptButton::cancel("Keep editing"),
                    PromptButton::new("Discard changes"),
                ],
                cx,
            );
            cx.spawn_in(window, async move |view, cx| {
                let answer = answer.await;
                let _ = view.update_in(cx, |view, _, cx| {
                    view.plugins.confirming = false;
                    if matches!(answer, Ok(1))
                        && epoch == view.plugins.epoch
                        && generation == view.plugins.generation
                    {
                        view.plugin_apply_selection(&id);
                    }
                    cx.notify();
                });
            })
            .detach();
        } else {
            self.plugin_apply_selection(&id);
            cx.notify();
        }
    }
    fn plugin_apply_selection(&mut self, id: &str) {
        self.plugins.selected = self
            .plugins
            .catalog
            .as_ref()
            .and_then(|catalog| {
                catalog
                    .plugins
                    .iter()
                    .find(|plugin| plugin.manifest.id == id)
            })
            .cloned();
        self.plugins.grants = self
            .plugins
            .selected
            .as_ref()
            .map(|plugin| plugin.grants.iter().cloned().collect())
            .unwrap_or_default();
        self.plugins.generation = self.plugins.generation.saturating_add(1);
    }
    fn plugin_install(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.plugins.retry.is_some() || self.plugins.busy() || self.plugins.grants_dirty() {
            return;
        }
        let Some(inspection) = self.plugins.inspection.clone() else {
            return;
        };
        let Some(catalog) = &self.plugins.catalog else {
            return;
        };
        let replace = catalog
            .plugins
            .iter()
            .any(|plugin| plugin.manifest.id == inspection.manifest.id);
        if replace {
            let epoch = self.plugins.epoch;
            let digest = inspection.package_sha256.clone();
            self.plugins.confirming = true;
            let answer = window.prompt(
                PromptLevel::Warning,
                "Update this plugin?",
                Some("The reviewed version and permissions will replace the installed version. Existing artifacts stay unchanged."),
                &[PromptButton::cancel("Cancel"), PromptButton::new("Update plugin")],
                cx,
            );
            cx.spawn_in(window, async move |view, cx| {
                let answer = answer.await;
                let _ = view.update_in(cx, |view, _, cx| {
                    view.plugins.confirming = false;
                    if matches!(answer, Ok(1))
                        && epoch == view.plugins.epoch
                        && view
                            .plugins
                            .inspection
                            .as_ref()
                            .is_some_and(|inspection| inspection.package_sha256 == digest)
                    {
                        view.plugin_dispatch_install(true, cx);
                    }
                    cx.notify();
                });
            })
            .detach();
        } else {
            self.plugin_dispatch_install(false, cx);
        }
    }
    fn plugin_dispatch_install(&mut self, replace: bool, cx: &mut Context<Self>) {
        let (Some(inspection), Some(path), Some(catalog)) = (
            &self.plugins.inspection,
            &self.plugins.manifest,
            &self.plugins.catalog,
        ) else {
            return;
        };
        let operation_id = Uuid::new_v4();
        let digest = inspection.package_sha256.clone();
        let args = json!({
            "operation_id":operation_id,
            "expected_revision":catalog.revision,
            "manifest_path":path,
            "expected_package_sha256":digest,
            "grants":self.plugins.review_grants,
            "enabled":self.plugins.enable_install,
            "replace":replace
        });
        self.plugins.notice = "Installing the reviewed plugin…".into();
        self.plugin_request(
            "helper_plugin_install",
            args,
            RequestKind::Mutation {
                operation_id,
                inspection: Some(digest),
            },
            cx,
        );
    }
    fn plugin_set_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        let (Some(plugin), Some(catalog)) = (&self.plugins.selected, &self.plugins.catalog) else {
            return;
        };
        if self.plugins.retry.is_some()
            || self.plugins.busy()
            || self.plugins.selected_changed()
            || self.plugins.grants_dirty()
        {
            return;
        }
        let operation_id = Uuid::new_v4();
        let args = json!({"operation_id":operation_id,"expected_revision":catalog.revision,"id":plugin.manifest.id,"enabled":enabled});
        self.plugin_request(
            "helper_plugin_set_enabled",
            args,
            RequestKind::Mutation {
                operation_id,
                inspection: None,
            },
            cx,
        );
    }
    fn plugin_save_grants(&mut self, cx: &mut Context<Self>) {
        if self.plugins.retry.is_some() || self.plugins.busy() || self.plugins.selected_changed() {
            return;
        }
        let (Some(plugin), Some(catalog)) = (&self.plugins.selected, &self.plugins.catalog) else {
            return;
        };
        let operation_id = Uuid::new_v4();
        let args = json!({"operation_id":operation_id,"expected_revision":catalog.revision,"id":plugin.manifest.id,"grants":self.plugins.grants});
        self.plugin_request(
            "helper_plugin_set_grants",
            args,
            RequestKind::Mutation {
                operation_id,
                inspection: None,
            },
            cx,
        );
    }
    fn plugin_confirm(&mut self, remove: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.plugins.retry.is_some() || self.plugins.busy() {
            return;
        }
        let Some(plugin) = &self.plugins.selected else {
            return;
        };
        let id = plugin.manifest.id.clone();
        let digest = plugin.package_sha256.clone();
        let epoch = self.plugins.epoch;
        self.plugins.confirming = true;
        let title = if remove {
            "Remove this plugin?"
        } else {
            "Revoke all permissions?"
        };
        let detail = if remove {
            "The plugin will no longer be available. Files it already created stay in Artifacts."
        } else {
            "The plugin will lose permission to access files and allowed websites. Its existing artifacts stay unchanged."
        };
        let answer = window.prompt(
            PromptLevel::Warning,
            title,
            Some(detail),
            &[
                PromptButton::cancel("Cancel"),
                PromptButton::new(if remove {
                    "Remove plugin"
                } else {
                    "Revoke permissions"
                }),
            ],
            cx,
        );
        cx.spawn_in(window, async move |view, cx| {
            let answer = answer.await;
            let _ = view.update_in(cx, |view, _, cx| {
                view.plugins.confirming = false;
                if matches!(answer, Ok(1))
                    && view.plugins.epoch == epoch
                    && view.plugins.selected.as_ref().is_some_and(|plugin| {
                        plugin.manifest.id == id && plugin.package_sha256 == digest
                    })
                {
                    if remove {
                        view.plugin_remove(cx);
                    } else {
                        view.plugins.grants.clear();
                        view.plugin_save_grants(cx);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
    fn plugin_remove(&mut self, cx: &mut Context<Self>) {
        let (Some(plugin), Some(catalog)) = (&self.plugins.selected, &self.plugins.catalog) else {
            return;
        };
        let operation_id = Uuid::new_v4();
        let args = json!({"operation_id":operation_id,"expected_revision":catalog.revision,"id":plugin.manifest.id});
        self.plugin_request(
            "helper_plugin_remove",
            args,
            RequestKind::Mutation {
                operation_id,
                inspection: None,
            },
            cx,
        );
    }
    fn plugin_run(&mut self, cx: &mut Context<Self>) {
        let Some(plugin) = &self.plugins.selected else {
            return;
        };
        if self.plugins.retry.is_some()
            || self.plugins.busy()
            || !plugin.enabled
            || self.plugins.grants_dirty()
            || self.plugins.selected_changed()
            || !self.workspace_ready
        {
            return;
        }
        let args = json!({"id":plugin.manifest.id,"package_sha256":plugin.package_sha256,"input":self.plugins.input.read(cx).value().to_string()});
        self.plugins.notice = "Running plugin…".into();
        self.plugin_request("plugin_run_installed", args, RequestKind::Run, cx);
    }
    fn plugin_cancel(&mut self, cx: &mut Context<Self>) {
        for id in self.plugins.pending.keys() {
            if let Some(host) = &self.host {
                let _ = host.cancel_capability(*id);
            }
        }
        self.plugins.notice =
            "Cancelling the request. Refresh the installed list before trying again.".into();
        cx.notify();
    }
    fn plugin_retry_change(&mut self, cx: &mut Context<Self>) {
        if self.plugins.busy() {
            return;
        }
        let Some(retry) = self.plugins.retry.clone() else {
            return;
        };
        self.plugins.notice = "Checking the previous settings change…".into();
        self.plugin_request(&retry.tool, retry.args, retry.kind, cx);
    }
    fn plugin_dismiss_change(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.plugins.busy() || self.plugins.retry.is_none() {
            return;
        }
        let epoch = self.plugins.epoch;
        self.plugins.confirming = true;
        let answer = window.prompt(
            PromptLevel::Warning,
            "Dismiss this local settings request?",
            Some("This does not undo a change already saved. Check the refreshed installed list before making another change."),
            &[PromptButton::cancel("Cancel"), PromptButton::new("Dismiss request")],
            cx,
        );
        cx.spawn_in(window, async move |view, cx| {
            let answer = answer.await;
            let _ = view.update_in(cx, |view, _, cx| {
                view.plugins.confirming = false;
                if matches!(answer, Ok(1)) && epoch == view.plugins.epoch {
                    view.plugins.retry = None;
                    view.plugins.notice = "Local request dismissed. Saved plugin settings stay as shown in the installed list.".into();
                }
                cx.notify();
            });
        }).detach();
    }
    pub(super) fn discard_plugin_setup(&mut self) {
        self.plugins.retry = None;
        self.plugins.inspection = None;
        self.plugins.manifest = None;
        self.plugins.review_grants.clear();
        self.plugins.enable_install = false;
        if let Some(plugin) = &self.plugins.selected {
            self.plugins.grants = plugin.grants.iter().cloned().collect();
        }
        self.plugins.generation = self.plugins.generation.saturating_add(1);
    }
    pub(super) fn plugin_result(
        &mut self,
        id: Uuid,
        result: Result<Value, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.plugins.retired.remove(&id) {
            return;
        }
        let Some(request) = self.plugins.pending.remove(&id) else {
            return;
        };
        if request.epoch != self.plugins.epoch {
            return;
        }
        let value = match result {
            Ok(value) => value,
            Err(error) => {
                if matches!(request.kind, RequestKind::Mutation { .. }) {
                    self.plugins.retry = Some(Retry {
                        tool: request.tool,
                        args: request.args,
                        kind: request.kind.clone(),
                    });
                }
                self.plugins.notice = plugin_error(&error);
                if matches!(request.kind, RequestKind::Run) {
                    Self::set_plugin_output(
                        &self.plugins.output,
                        self.plugins.notice.clone(),
                        window,
                        cx,
                    );
                }
                if !matches!(request.kind, RequestKind::List) {
                    self.refresh_plugins(cx);
                }
                cx.notify();
                return;
            }
        };
        let summary = value
            .get("summary")
            .and_then(Value::as_str)
            .and_then(|text| serde_json::from_str::<Value>(text).ok())
            .unwrap_or_else(|| value.clone());
        let control_retry = matches!(request.kind, RequestKind::Mutation { .. }).then(|| Retry {
            tool: request.tool.clone(),
            args: request.args.clone(),
            kind: request.kind.clone(),
        });
        let result: Result<(), String> = (|| {
            match request.kind {
                RequestKind::List => {
                    let catalog: PluginCatalog = serde_json::from_value(summary)
                        .map_err(|_| "The installed plugin list could not be read.".to_string())?;
                    self.plugin_accept_catalog(catalog, false);
                }
                RequestKind::Inspect(path) => {
                    let inspection: PluginInspection = serde_json::from_value(summary)
                        .map_err(|_| "The selected package could not be reviewed.".to_string())?;
                    self.plugins.inspection = Some(inspection);
                    self.plugins.manifest = Some(path);
                    self.plugins.review_grants.clear();
                    self.plugins.generation = self.plugins.generation.saturating_add(1);
                    self.plugins.notice="Review the requested permissions, then choose Install. Installing does not run the plugin.".into();
                }
                RequestKind::Mutation {
                    operation_id,
                    inspection,
                } => {
                    let receipt: PluginManagementResult =
                        serde_json::from_value(summary).map_err(|_| {
                            "The saved result could not be confirmed. Refresh the installed list."
                                .to_string()
                        })?;
                    if receipt.operation_id != operation_id {
                        return Err(
                            "The saved result could not be confirmed. Refresh the installed list."
                                .into(),
                        );
                    }
                    self.plugins.retry = None;
                    self.plugin_accept_catalog(receipt.catalog, true);
                    self.plugins.notice = receipt.message;
                    if inspection.as_ref().is_some_and(|digest| {
                        self.plugins
                            .inspection
                            .as_ref()
                            .is_some_and(|review| &review.package_sha256 == digest)
                    }) {
                        self.plugins.inspection = None;
                        self.plugins.manifest = None;
                        self.plugins.review_grants.clear();
                    }
                }
                RequestKind::Run => {
                    let fallback = summary
                        .get("error")
                        .and_then(Value::as_str)
                        .map(plugin_error)
                        .unwrap_or_default();
                    let output = summary
                        .get("output")
                        .and_then(Value::as_str)
                        .unwrap_or(&fallback);
                    Self::set_plugin_output(&self.plugins.output, output.to_owned(), window, cx);
                    self.plugins.artifacts = value
                        .get("artifacts")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(PathBuf::from)
                        .collect();
                    for path in self.plugins.artifacts.clone() {
                        self.record_artifact(path);
                    }
                    self.plugins.notice = match value.get("status").and_then(Value::as_str) {
                        Some("failed") => "The plugin stopped before completing. Check its output and Artifacts before trying again.",
                        Some("outcome_unknown") => "The plugin result needs checking. Inspect its output and Artifacts before trying again.",
                        _ => "Plugin finished."
                    }.into();
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            if control_retry.is_some() {
                self.plugins.retry = control_retry;
                self.refresh_plugins(cx);
            }
            self.plugins.notice = error;
        }
        cx.notify();
    }
    fn set_plugin_output(
        input: &Entity<TextareaState>,
        value: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        input.update(cx, |input, cx| input.set_value(value, window, cx));
    }
    fn plugin_accept_catalog(&mut self, catalog: PluginCatalog, mutation: bool) {
        if let Some(selected) = &self.plugins.selected {
            let replacement = catalog
                .plugins
                .iter()
                .find(|plugin| plugin.manifest.id == selected.manifest.id)
                .cloned();
            if mutation || !self.plugins.grants_dirty() {
                self.plugins.selected = replacement;
                self.plugins.grants = self
                    .plugins
                    .selected
                    .as_ref()
                    .map(|plugin| plugin.grants.iter().cloned().collect())
                    .unwrap_or_default();
            }
        }
        self.plugins.catalog = Some(catalog);
    }
}
fn permission_label(value: &str) -> String {
    match value {
        "read_workspace" => "Read files in the selected project".into(),
        "write_artifacts" => "Create files in Artifacts".into(),
        value if value.starts_with("https:") => format!("Access {} over HTTPS", &value[6..]),
        _ => "Unrecognized permission".into(),
    }
}
fn plugin_error(error: &str) -> String {
    if error.contains("revision") || error.contains("changed") {
        "Plugin settings or package files changed. Refresh and review them again.".into()
    } else if error.contains("cancel") {
        "Request cancelled. Refresh to check the installed state before retrying.".into()
    } else if error.contains("grant") || error.contains("permission") || error.contains("disabled")
    {
        "The plugin could not run with its current permissions. Review its enabled state and permissions.".into()
    } else if error.contains("busy") {
        "The plugin is still running. Wait or cancel it before changing its settings.".into()
    } else {
        "The plugin request could not finish. Check the selected package and permissions, then try again.".into()
    }
}

#[cfg(test)]
#[allow(dead_code)]
impl Xylo {
    pub(super) fn validation_plugin_catalog(&self) -> Option<PluginCatalog> {
        self.plugins.catalog.clone()
    }
    pub(super) fn validation_plugin_review(&self) -> Option<PluginInspection> {
        self.plugins.inspection.clone()
    }
    pub(super) fn validation_plugin_idle(&self) -> bool {
        self.plugins.pending.is_empty() && !self.plugins.confirming
    }
    pub(super) fn validation_plugin_output(&self, cx: &App) -> String {
        self.plugins.output.read(cx).value().to_string()
    }
    pub(super) fn validation_plugin_artifacts(&self) -> Vec<PathBuf> {
        self.plugins.artifacts.clone()
    }
}
