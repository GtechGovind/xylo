//! Portable extension management. Filesystem and networking stay on the host.
//!
//! "Plugins" are portable packages (skills, commands, agents, hooks, MCP and
//! language servers); "Native modules" are Xylo's own WebAssembly plugins.
//! A package is reviewed before it runs: an install opens a review of what it
//! contains, and enabling requires confirming that review.
//!
//! Package operations are single-flight. Discover reads the host's cached
//! plugin directory (`extension_directory`, cheap enough for every keystroke)
//! and refreshes that cache from GitHub in the background
//! (`extension_directory_refresh`); both run beside package operations, so
//! browsing never blocks installing or configuring. The page itself is in
//! `extensions_view.rs`.
use super::*;
use serde_json::{Value, json};
use std::time::Instant;
use xylo_runtime::extensions::{
    ComponentKind, DirectoryEntry, DirectoryPage, DirectoryQuery, DirectorySourceStatus,
    ExtensionPackage, ExtensionSource, default_sources,
};
#[path = "extension_setup.rs"]
mod setup;
#[path = "extensions_view.rs"]
mod view;

/// Tabs of the Plugins page, in segmented-control order; Sources is the
/// third.
pub(super) const DISCOVER: usize = 0;
pub(super) const INSTALLED: usize = 1;

/// The plugin a detail sub-page shows.
#[derive(Clone)]
enum Detail {
    /// A directory entry, installed or not.
    Available(Box<DirectoryEntry>),
    /// An installed package, by id.
    Installed(String),
}

/// Directory results come from the host's cache, so a search runs on every
/// keystroke; refreshing the cache from GitHub runs in the background. Both
/// sit beside the single-flight package operations so browsing never blocks
/// installing or configuring.
struct Directory {
    /// The page for the current search, once one has arrived.
    page: Option<DirectoryPage>,
    /// The newest search; older answers are ignored.
    search: Option<Uuid>,
    /// Installed entries, for the update pills of the Installed tab.
    installed: Vec<DirectoryEntry>,
    installed_search: Option<Uuid>,
    /// The background refresh in flight.
    refresh: Option<Uuid>,
    /// Source freshness from the latest refresh or search.
    sources: Vec<DirectorySourceStatus>,
    /// Why the last search or refresh request failed.
    error: Option<String>,
    /// Category filter; `None` shows every category.
    category: Option<String>,
    /// When the wait for the first page began, for the slow caption.
    since: Option<Instant>,
    /// When a forced refresh began, for the slow caption.
    refresh_since: Option<Instant>,
    /// Entries requested; "Show more" grows it by a page.
    limit: usize,
}
impl Default for Directory {
    fn default() -> Self {
        Self {
            page: None,
            search: None,
            installed: Vec::new(),
            installed_search: None,
            refresh: None,
            sources: Vec::new(),
            error: None,
            category: None,
            since: None,
            refresh_since: None,
            limit: DIRECTORY_PAGE,
        }
    }
}
/// Entries per directory page.
const DIRECTORY_PAGE: usize = 50;

pub(super) struct ExtensionUi {
    tab: usize,
    native: bool,
    packages: Vec<ExtensionPackage>,
    sources: Vec<ExtensionSource>,
    pending: Option<Uuid>,
    pending_name: String,
    /// The entry, package or source the pending operation acts on, so only
    /// its button shows as busy.
    pending_target: Option<String>,
    /// When the pending operation began, for the slow caption.
    pending_since: Option<Instant>,
    cancelling: bool,
    loaded: bool,
    notice: String,
    source: Entity<InputState>,
    query: Entity<InputState>,
    /// Repository shown in Discover; `None` shows every source.
    source_filter: Option<String>,
    directory: Directory,
    detail: Option<Detail>,
    /// Whether the Sources tab shows its add-source form.
    adding_source: bool,
    pub(super) setup: Option<setup::SetupUi>,
    /// The package whose contents the user is reviewing before enabling it.
    review: Option<ExtensionPackage>,
    /// Holds focus while Plugins swaps sub-pages; see `focus_plugins_page`.
    focus: FocusHandle,
    _subscriptions: Vec<Subscription>,
}
impl ExtensionUi {
    pub fn new(window: &mut Window, cx: &mut Context<Xylo>) -> Self {
        let input = |placeholder: &'static str, window: &mut Window, cx: &mut Context<Xylo>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        let query = input("Search plugins and skills", window, cx);
        // Discover searches the cached directory as the person types.
        let subscription = cx.subscribe(&query, |view, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                view.extensions.directory.limit = DIRECTORY_PAGE;
                view.extension_directory_search(cx);
            }
        });
        Self {
            tab: DISCOVER,
            native: false,
            packages: vec![],
            sources: default_sources(),
            pending: None,
            pending_name: String::new(),
            pending_target: None,
            pending_since: None,
            cancelling: false,
            loaded: false,
            notice: String::new(),
            source: input("owner/repository or a GitHub folder URL", window, cx),
            query,
            source_filter: None,
            directory: Directory::default(),
            detail: None,
            adding_source: false,
            setup: None,
            review: None,
            focus: cx.focus_handle(),
            _subscriptions: vec![subscription],
        }
    }
    pub(super) fn active(&self) -> bool {
        self.pending.is_some()
    }
}

/// One portable skill from an installed package, for the Skills page.
pub(super) struct InstalledSkill {
    pub(super) package_id: String,
    /// Readable name of the package that ships the skill.
    pub(super) package: String,
    pub(super) name: String,
    pub(super) description: String,
    /// Whether its package is enabled, so the agent can load it.
    pub(super) enabled: bool,
}

/// Identity of a source: the same repository can be added at another
/// revision or folder.
fn source_key(source: &ExtensionSource) -> String {
    format!(
        "{}@{}/{}",
        source.repository, source.revision, source.subdirectory
    )
}
/// Whether a source ships with Xylo; only custom sources can be removed.
fn built_in_source(source: &ExtensionSource) -> bool {
    default_sources()
        .iter()
        .any(|built_in| source_key(built_in) == source_key(source))
}

/// A readable name for a package or skill id: words split on `-`, `_` and
/// `.`, the first capitalised, common acronyms and brands spelled properly
/// ("build-ios-apps" -> "Build iOS apps").
pub(super) fn display_name(raw: &str) -> String {
    const SPELLINGS: [(&str, &str); 18] = [
        ("ios", "iOS"),
        ("macos", "macOS"),
        ("api", "API"),
        ("apis", "APIs"),
        ("mcp", "MCP"),
        ("lsp", "LSP"),
        ("ui", "UI"),
        ("ux", "UX"),
        ("cli", "CLI"),
        ("sql", "SQL"),
        ("pdf", "PDF"),
        ("css", "CSS"),
        ("html", "HTML"),
        ("github", "GitHub"),
        ("gitlab", "GitLab"),
        ("aws", "AWS"),
        ("ci", "CI"),
        ("json", "JSON"),
    ];
    let words: Vec<String> = raw
        .split(['-', '_', '.', ' '])
        .filter(|word| !word.is_empty())
        .enumerate()
        .map(|(index, word)| {
            let lower = word.to_lowercase();
            if let Some((_, spelling)) = SPELLINGS.iter().find(|(key, _)| *key == lower) {
                return (*spelling).to_owned();
            }
            if index > 0 || word.chars().any(char::is_uppercase) {
                return if index == 0 { word.to_owned() } else { lower };
            }
            let mut chars = word.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().chain(chars).collect())
                .unwrap_or_default()
        })
        .collect();
    if words.is_empty() {
        raw.to_owned()
    } else {
        words.join(" ")
    }
}

/// "5 min ago" for a past time in Unix seconds.
fn time_ago(then: i64) -> String {
    let seconds = (now_ms() / 1000).saturating_sub(then).max(0);
    match seconds {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", seconds / 60),
        3600..=86399 => format!("{} h ago", seconds / 3600),
        _ => format!("{} days ago", seconds / 86400),
    }
}
/// The freshness caption for the shown sources: the oldest fetch among them,
/// or why there is nothing yet.
fn freshness_caption(statuses: &[&DirectorySourceStatus], refreshing: bool) -> String {
    if refreshing {
        return "Checking sources for updates…".into();
    }
    match statuses.iter().filter_map(|status| status.fetched_at).min() {
        Some(oldest) => format!("Updated {}", time_ago(oldest)),
        None => "Not loaded yet".into(),
    }
}

impl Xylo {
    pub(super) fn extension_request(&mut self, name: &str, args: Value, cx: &mut Context<Self>) {
        if self.extensions.pending.is_some() {
            return;
        }
        let id = Uuid::new_v4();
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.capability(id, name.into(), args));
        match result {
            Ok(()) => {
                self.extensions.pending_since = Some(self.arm_slow_hint(cx));
                self.extensions.pending_target = None;
                self.extensions.pending = Some(id);
                self.extensions.pending_name = name.into();
                self.extensions.cancelling = false;
                self.extensions.notice.clear();
            }
            Err(e) => self.extensions.notice = product_error(&e.to_string()),
        }
        cx.notify();
    }
    /// Starts an operation on one entry, package or source so that only its
    /// button shows as busy.
    fn extension_request_for(
        &mut self,
        name: &str,
        target: impl Into<String>,
        args: Value,
        cx: &mut Context<Self>,
    ) {
        let idle = self.extensions.pending.is_none();
        self.extension_request(name, args, cx);
        if idle && self.extensions.pending.is_some() {
            self.extensions.pending_target = Some(target.into());
        }
    }
    /// Whether `name` is running, on `target` when one is given.
    fn extension_busy(&self, name: &str, target: Option<&str>) -> bool {
        self.extensions.pending.is_some()
            && self.extensions.pending_name == name
            && target.is_none_or(|target| self.extensions.pending_target.as_deref() == Some(target))
    }
    fn extension_directory_query(&self, installed_only: bool, cx: &App) -> DirectoryQuery {
        let ui = &self.extensions;
        DirectoryQuery {
            text: if installed_only {
                String::new()
            } else {
                ui.query.read(cx).value().trim().to_owned()
            },
            source: if installed_only {
                None
            } else {
                ui.source_filter.clone()
            },
            category: if installed_only {
                None
            } else {
                ui.directory.category.clone()
            },
            installed_only,
            offset: 0,
            limit: if installed_only {
                200
            } else {
                ui.directory.limit
            },
        }
    }
    /// Sends a capability call beside the package operations; returns its id.
    fn extension_directory_call(&mut self, name: &str, args: Value) -> Option<Uuid> {
        let id = Uuid::new_v4();
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.capability(id, name.into(), args));
        match result {
            Ok(()) => Some(id),
            Err(error) => {
                self.extensions.directory.error = Some(product_error(&error.to_string()));
                None
            }
        }
    }
    /// Searches the cached directory with the current text, source and
    /// category; the newest search wins.
    pub(super) fn extension_directory_search(&mut self, cx: &mut Context<Self>) {
        if self.extensions.directory.page.is_none() && self.extensions.directory.since.is_none() {
            self.extensions.directory.since = Some(self.arm_slow_hint(cx));
        }
        let query = self.extension_directory_query(false, cx);
        self.extensions.directory.search =
            self.extension_directory_call("extension_directory", json!(query));
        cx.notify();
    }
    /// Loads the installed entries that the Installed tab's pills need.
    fn extension_directory_installed(&mut self, cx: &mut Context<Self>) {
        let query = self.extension_directory_query(true, cx);
        self.extensions.directory.installed_search =
            self.extension_directory_call("extension_directory", json!(query));
        cx.notify();
    }
    /// Refreshes source caches from GitHub in the background; `force` also
    /// refreshes caches that are still fresh.
    pub(super) fn extension_directory_refresh(&mut self, force: bool, cx: &mut Context<Self>) {
        if self.extensions.directory.refresh.is_some() {
            return;
        }
        let query = self.extension_directory_query(false, cx);
        self.extensions.directory.error = None;
        self.extensions.directory.refresh_since = force.then(|| self.arm_slow_hint(cx));
        self.extensions.directory.refresh = self.extension_directory_call(
            "extension_directory_refresh",
            json!({"force": force, "query": query}),
        );
        cx.notify();
    }
    /// Applies a directory answer; returns false when `id` is not a
    /// directory request.
    fn extension_directory_result(
        &mut self,
        id: Uuid,
        result: &Result<Value, String>,
        cx: &mut Context<Self>,
    ) -> bool {
        let directory = &mut self.extensions.directory;
        let kind = if directory.search == Some(id) {
            directory.search = None;
            0
        } else if directory.installed_search == Some(id) {
            directory.installed_search = None;
            1
        } else if directory.refresh == Some(id) {
            directory.refresh = None;
            2
        } else {
            return false;
        };
        let parsed = result
            .clone()
            .map_err(anyhow::Error::msg)
            .and_then(|value| {
                let page: DirectoryPage = serde_json::from_value(value["directory"].clone())?;
                let sources: Option<Vec<DirectorySourceStatus>> = value
                    .get("sources")
                    .map(|sources| serde_json::from_value(sources.clone()))
                    .transpose()?;
                Ok((page, sources))
            });
        match parsed {
            Ok((page, sources)) => {
                directory.error = None;
                directory.sources = sources.unwrap_or_else(|| page.sources.clone());
                if kind == 1 {
                    directory.installed = page.entries;
                } else {
                    directory.since = None;
                    directory.page = Some(page);
                }
            }
            Err(error) => directory.error = Some(product_error(&error.to_string())),
        }
        if kind == 2 {
            // The cache changed: the installed entries may have updates now.
            self.extension_directory_installed(cx);
        }
        cx.notify();
        true
    }
    /// Opening Plugins: search the cache now, then refresh it in the
    /// background.
    pub(super) fn extension_directory_open(&mut self, cx: &mut Context<Self>) {
        self.extension_directory_search(cx);
        self.extension_directory_installed(cx);
        self.extension_directory_refresh(false, cx);
    }
    /// The directory entry of an installed package, when a source lists it.
    fn extension_directory_entry(&self, package: &ExtensionPackage) -> Option<&DirectoryEntry> {
        self.extensions.directory.installed.iter().find(|entry| {
            entry.install.as_ref().is_some_and(|item| {
                package.source.ends_with(&format!("/{}", item.path))
                    && package.source.contains(&item.source.repository)
            })
        })
    }
    /// Installed portable skills across packages, in package order.
    pub(super) fn installed_skills(&self) -> Vec<InstalledSkill> {
        self.extensions
            .packages
            .iter()
            .flat_map(|package| {
                package
                    .components
                    .iter()
                    .filter(|component| component.kind == ComponentKind::Skill)
                    .map(move |component| InstalledSkill {
                        package_id: package.id.clone(),
                        package: display_name(&package.name),
                        name: component.name.clone(),
                        description: component.description.clone(),
                        enabled: package.enabled,
                    })
            })
            .collect()
    }
    /// When the pending package operation began, for the slow caption.
    pub(super) fn extensions_pending_since(&self) -> Option<Instant> {
        self.extensions.pending_since
    }
    /// Whether the installed packages have been loaded at least once.
    pub(super) fn extensions_loaded(&self) -> bool {
        self.extensions.loaded
    }
    /// Loads installed packages unless they are already known.
    pub(super) fn ensure_extensions_loaded(&mut self, cx: &mut Context<Self>) {
        if !self.extensions.loaded {
            self.extension_request("extension_snapshot", json!({}), cx);
        }
    }
    /// "Plugins / Native modules" above the native modules sub-page.
    pub(super) fn native_modules_breadcrumb(&self, cx: &mut Context<Self>) -> Div {
        settings_ui::breadcrumb(
            [
                settings_ui::crumb("native-modules-back", "Plugins").on_click(cx.listener(
                    |view, _, window, cx| {
                        view.focus_plugins_page(window, cx);
                        view.extensions.native = false;
                        cx.notify();
                    },
                )),
            ],
            "Native modules",
        )
    }
    #[cfg(test)]
    #[allow(dead_code)] // Exercised by the separate native validation test target.
    pub(crate) fn validation_extensions_idle(&self) -> bool {
        self.extensions.pending.is_none()
    }
    #[cfg(test)]
    #[allow(dead_code)] // Exercised by the separate native validation test target.
    pub(crate) fn validation_extension_setup_open(&self) -> bool {
        self.extensions.setup.is_some()
    }
    /// Moves focus to the Plugins page itself. A sub-page change removes the
    /// control that was pressed, and focus left on a control that is gone
    /// stops Tab from moving; from the page, Tab continues with the new
    /// sub-page's first control.
    pub(super) fn focus_plugins_page(&self, window: &mut Window, cx: &mut App) {
        self.extensions.focus.focus(window, cx);
    }
    /// Opens Plugins on its Discover tab, for "Browse plugins" elsewhere.
    pub(super) fn open_plugins_discover(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.extensions.native = false;
        self.extensions.detail = None;
        self.extension_leave_setup(Some(DISCOVER), window, cx);
        self.navigate(Page::Plugins, cx);
    }
    fn confirm_remove_extension(
        &mut self,
        package: &ExtensionPackage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = package.id.clone();
        self.confirm_destructive(
            &format!("Remove {}?", package.name),
            "Its skills, commands, hooks and servers are removed with their setup.",
            "Remove plugin",
            window,
            cx,
            move |view, _, cx| {
                if view.extensions.review.as_ref().is_some_and(|p| p.id == id) {
                    view.extensions.review = None;
                }
                view.extension_request("extension_remove", json!({"id": id}), cx)
            },
        );
    }
    pub(super) fn extension_result(
        &mut self,
        id: Uuid,
        result: Result<Value, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.extension_directory_result(id, &result, cx) {
            return;
        }
        if self.extensions.pending != Some(id) {
            return;
        }
        self.extensions.pending = None;
        let operation = std::mem::take(&mut self.extensions.pending_name);
        let cancelling = std::mem::take(&mut self.extensions.cancelling);
        let mut sources_changed = false;
        let result = result.map_err(anyhow::Error::msg).and_then(|v| {
            self.extension_setup_result(&v, window, cx)?;
            if let Some(p) = v.get("packages") {
                let previous = std::mem::replace(
                    &mut self.extensions.packages,
                    serde_json::from_value(p.clone())?,
                );
                // A local install answers with the package list only; the
                // package it installed or replaced is the one that is new or
                // whose contents changed, and it opens for review like any
                // install.
                if operation == "extension_install_local"
                    && let Some(package) = installed_package(&previous, &self.extensions.packages)
                {
                    self.extensions.review = Some(package.clone());
                    self.extensions.notice = install_notice(package);
                }
                self.extensions.loaded = true;
                // A removed plugin's page closes with it.
                if let Some(Detail::Installed(id)) = &self.extensions.detail
                    && !self
                        .extensions
                        .packages
                        .iter()
                        .any(|package| &package.id == id)
                {
                    self.extensions.detail = None;
                }
            }
            if let Some(s) = v.get("sources") {
                self.extensions.sources = serde_json::from_value(s.clone())?;
                if self
                    .extensions
                    .source_filter
                    .as_ref()
                    .is_some_and(|filter| {
                        !self
                            .extensions
                            .sources
                            .iter()
                            .any(|source| &source.repository == filter)
                    })
                {
                    self.extensions.source_filter = None;
                }
                sources_changed = true;
            }
            if let Some(package) = v.get("package") {
                let package: ExtensionPackage = serde_json::from_value(package.clone())?;
                self.extensions.notice = install_notice(&package);
                self.extensions.review = Some(package);
            }
            Ok(())
        });
        if sources_changed && operation == "extension_sources" {
            self.extensions.adding_source = false;
            self.extension_directory_refresh(false, cx);
        }
        if operation != "extension_snapshot" && operation != "extension_component_status" {
            // Installs, removals and enables change what the directory marks
            // as installed.
            self.extension_directory_search(cx);
            self.extension_directory_installed(cx);
        }
        if let Err(e) = result {
            let cause = product_error(&e.to_string());
            self.extensions.notice = if cancelling {
                format!(
                    "Operation interrupted. Changes made before cancellation may remain. {cause}"
                )
            } else if operation == "extension_component_verify" {
                format!(
                    "Verification failed. Check the component's command and setup, then retry. {cause}"
                )
            } else {
                cause
            };
        }
        cx.notify();
    }
    fn extension_pick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.extensions.pending.is_some() {
            return;
        }
        cx.spawn_in(window, async move |view, cx| {
            if let Some(folder) = rfd::AsyncFileDialog::new()
                .set_title("Choose a skill or plugin folder")
                .pick_folder()
                .await
            {
                let _ = view.update_in(cx, |view, _, cx| {
                    view.extension_request(
                        "extension_install_local",
                        json!({"path": folder.path()}),
                        cx,
                    )
                });
            }
        })
        .detach();
    }
}

/// The package an install added or replaced: new, or with changed contents.
fn installed_package<'a>(
    previous: &[ExtensionPackage],
    current: &'a [ExtensionPackage],
) -> Option<&'a ExtensionPackage> {
    current.iter().find(|package| {
        !previous
            .iter()
            .any(|old| old.id == package.id && old.digest == package.digest)
    })
}
/// What to do after an install: review before enabling, or review what an
/// update changed in an enabled plugin.
fn install_notice(package: &ExtensionPackage) -> String {
    if package.enabled {
        "Updated. This plugin stays enabled; review what changed below.".into()
    } else {
        "Installed and disabled. Review what it contains before enabling it.".into()
    }
}
/// Product name of a component group.
fn component_group(kind: &ComponentKind) -> &'static str {
    match kind {
        ComponentKind::Skill => "Skills",
        ComponentKind::Command => "Commands",
        ComponentKind::Agent => "Agents",
        ComponentKind::Hooks => "Hooks",
        ComponentKind::Mcp => "MCP servers",
        ComponentKind::LanguageServer => "Language servers",
        ComponentKind::HostedIntegration => "Hosted integrations",
    }
}
/// Short tag for a component kind in list rows.
fn component_tag(kind: &ComponentKind) -> &'static str {
    match kind {
        ComponentKind::Skill => "Skills",
        ComponentKind::Command => "Commands",
        ComponentKind::Agent => "Agents",
        ComponentKind::Hooks => "Hooks",
        ComponentKind::Mcp => "MCP",
        ComponentKind::LanguageServer => "LSP",
        ComponentKind::HostedIntegration => "Hosted",
    }
}
/// Distinct component kinds of a package, in review order.
fn package_kinds(package: &ExtensionPackage) -> Vec<ComponentKind> {
    review_groups(package)
        .into_iter()
        .map(|(kind, _)| kind)
        .collect()
}
/// Whether a component runs code and so has a setup to configure.
fn configurable(kind: &ComponentKind) -> bool {
    matches!(
        kind,
        ComponentKind::Hooks | ComponentKind::Mcp | ComponentKind::LanguageServer
    )
}
/// What enabling a component group allows, in one line.
fn component_risk(kind: &ComponentKind) -> &'static str {
    match kind {
        ComponentKind::Skill => "Instructions the agent can load when relevant.",
        ComponentKind::Command => "Prompts you can run by name.",
        ComponentKind::Agent => "Helpers the agent can delegate work to.",
        ComponentKind::Hooks => {
            "Run local commands automatically around tool use, once configured and enabled per event."
        }
        ComponentKind::Mcp => "Run local server programs that provide tools.",
        ComponentKind::LanguageServer => "Run local language servers for code intelligence.",
        ComponentKind::HostedIntegration => "Need a separate connection before they can be used.",
    }
}
/// Indices of the package's components grouped by kind, in review order.
fn review_groups(package: &ExtensionPackage) -> Vec<(ComponentKind, Vec<usize>)> {
    const ORDER: [ComponentKind; 7] = [
        ComponentKind::Hooks,
        ComponentKind::Mcp,
        ComponentKind::LanguageServer,
        ComponentKind::Command,
        ComponentKind::Agent,
        ComponentKind::Skill,
        ComponentKind::HostedIntegration,
    ];
    ORDER
        .into_iter()
        .filter_map(|kind| {
            let rows: Vec<usize> = package
                .components
                .iter()
                .enumerate()
                .filter(|(_, component)| component.kind == kind)
                .map(|(index, _)| index)
                .collect();
            (!rows.is_empty()).then_some((kind, rows))
        })
        .collect()
}

#[cfg(test)]
#[path = "extensions/tests.rs"]
mod tests;
