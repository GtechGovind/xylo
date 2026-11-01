//! Focused settings destinations. Every control uses an existing host boundary.
//!
//! Layout comes from [`super::settings_ui`]; see `SETTINGS_DESIGN.md`.
use super::*;
use crate::icons::AppIcon;
use anyhow::Context as _;
use gpui_kit::TestSupportExt;
use xylo_runtime::settings_files::{PermissionDecision, PermissionRule};

/// Standing approvals of the open project, as shown on the Permissions page.
#[derive(Default)]
pub(super) struct PermissionsUi {
    /// The project whose rules are listed (or being loaded).
    workspace: Option<PathBuf>,
    rules: Vec<PermissionRule>,
    loading: bool,
    /// When the current load or revoke was sent, for the slow-wait caption.
    loading_since: Option<std::time::Instant>,
    error: Option<String>,
}
/// The tag next to a trusted connection's name in the Connections list.
const TRUSTED_TAG: &str = "Trusted";
fn secret_storage_label(storage: SecretStorage) -> &'static str {
    match storage {
        SecretStorage::LocalFile => "Local encrypted file",
        SecretStorage::OsKeychain => "System keychain",
    }
}
fn secret_storage_detail(storage: SecretStorage) -> &'static str {
    match storage {
        SecretStorage::LocalFile => {
            "Keys are encrypted in Xylo's data folder with a key file only your account can read. No system prompts."
        }
        SecretStorage::OsKeychain => {
            "The encryption key lives in your system keychain: stronger protection, but your system may ask for permission."
        }
    }
}

impl Xylo {
    /// The settings sidebar: back to the workspace, the "Settings" title, then
    /// every destination in groups.
    pub(super) fn settings_navigation(&self, cx: &mut Context<Self>) -> AnyElement {
        let header = div()
            .flex()
            .flex_col()
            .child(
                div().h(px(52.)).flex().items_center().child(
                    settings_ui::settings_nav_item(
                        "back-to-chat",
                        "Back to workspace",
                        AppIcon::Back,
                        false,
                    )
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.reset_provider_discovery(cx);
                        view.cancel_account_command(cx);
                        view.settings_open = false;
                        view.page = Page::Chat;
                        view.persist_ui(cx);
                        cx.notify();
                    })),
                ),
            )
            .child(
                div()
                    .px(px(10.))
                    .pt(px(4.))
                    .pb(px(12.))
                    .text_size(px(settings_ui::TITLE_SIZE - 2.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(text()))
                    .child("Settings"),
            );
        type Destinations = Vec<(Page, &'static str, &'static str, AppIcon)>;
        let groups: [(Option<&'static str>, Destinations); 5] = [
            (
                None,
                vec![
                    (Page::Appearance, "nav-general", "General", AppIcon::General),
                    (
                        Page::Permissions,
                        "nav-permissions",
                        "Permissions",
                        AppIcon::Permissions,
                    ),
                    (
                        Page::Notifications,
                        "nav-notifications",
                        "Notifications",
                        AppIcon::Notifications,
                    ),
                ],
            ),
            (
                Some("Models & accounts"),
                vec![
                    (
                        Page::Providers,
                        "nav-providers",
                        "Providers",
                        AppIcon::Model,
                    ),
                    (
                        Page::Images,
                        "nav-images",
                        "Image generation",
                        AppIcon::Image,
                    ),
                    (Page::Voice, "nav-voice", "Voice", AppIcon::Mic),
                ],
            ),
            (
                Some("Tools & integrations"),
                vec![
                    (
                        Page::Connectors,
                        "nav-connections",
                        "Connections",
                        AppIcon::Connectors,
                    ),
                    (Page::Plugins, "nav-plugins", "Plugins", AppIcon::Plugins),
                    (
                        Page::Environments,
                        "nav-environments",
                        "Environments",
                        AppIcon::Environment,
                    ),
                ],
            ),
            (
                Some("Knowledge"),
                vec![
                    (Page::Skills, "nav-skills", "Skills", AppIcon::Skills),
                    (Page::Memory, "nav-memory", "Memory", AppIcon::Memory),
                ],
            ),
            (
                None,
                vec![(Page::Assets, "nav-downloads", "Downloads", AppIcon::Storage)],
            ),
        ];
        let groups = groups
            .into_iter()
            .map(|(label, pages)| {
                let group = label.map_or_else(settings_ui::NavGroup::untitled, |label| {
                    settings_ui::NavGroup::new(label)
                });
                pages
                    .into_iter()
                    .fold(group, |group, (page, id, label, glyph)| {
                        group.item(
                            settings_ui::settings_nav_item(id, label, glyph, self.page == page)
                                .on_click(
                                    cx.listener(move |view, _, _, cx| view.navigate(page, cx)),
                                ),
                        )
                    })
            })
            .collect();
        settings_ui::settings_sidebar(header, groups).into_any_element()
    }
    /// Switching storage starts an empty vault there: saved keys and tokens
    /// are erased, never carried over, so the switch is confirmed first.
    fn confirm_secret_storage(
        &mut self,
        option: SecretStorage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.settings.secret_storage == option {
            return;
        }
        self.confirm_destructive(
            &format!("Switch to {}?", secret_storage_label(option)),
            "Saved provider keys and connector tokens will be removed and you'll need to reconnect.",
            "Switch",
            window,
            cx,
            move |view, _, cx| {
                view.settings.secret_storage = option;
                let settings = view.settings.clone();
                view.host_call(cx, |host| host.save_settings(settings));
                cx.notify();
            },
        );
    }
    /// Loads the open project's standing approvals.
    pub(super) fn refresh_approval_rules(&mut self, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.clone() else {
            self.permissions = PermissionsUi::default();
            cx.notify();
            return;
        };
        if self.permissions.workspace.as_ref() != Some(&workspace) {
            self.permissions.rules.clear();
        }
        self.permissions.workspace = Some(workspace.clone());
        self.permissions.error = None;
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.approval_rules(workspace));
        self.permissions.loading = result.is_ok();
        self.permissions.loading_since = result.is_ok().then(|| self.arm_slow_hint(cx));
        if let Err(error) = result {
            self.permissions.error = Some(error.to_string());
        }
        cx.notify();
    }
    fn revoke_approval_rule(&mut self, key: PermissionRule, cx: &mut Context<Self>) {
        let Some(workspace) = self.permissions.workspace.clone() else {
            return;
        };
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.remove_approval_rule(workspace, key));
        self.permissions.loading = result.is_ok();
        self.permissions.loading_since = result.is_ok().then(|| self.arm_slow_hint(cx));
        if let Err(error) = result {
            self.permissions.error = Some(error.to_string());
        }
        cx.notify();
    }
    /// Applies a project's rules unless the page has moved to another project.
    pub(super) fn approval_rules_received(
        &mut self,
        workspace: PathBuf,
        result: Result<Vec<PermissionRule>, String>,
        cx: &mut Context<Self>,
    ) {
        if self.permissions.workspace.as_ref() != Some(&workspace) {
            return;
        }
        self.permissions.loading = false;
        self.permissions.loading_since = None;
        match result {
            Ok(rules) => {
                self.permissions.rules = rules;
                self.permissions.error = None;
            }
            Err(error) => self.permissions.error = Some(error),
        }
        cx.notify();
    }
    pub(super) fn save_connector(&mut self, cx: &mut Context<Self>) {
        if self.connector_pending.is_some() {
            return;
        }
        let name = self.connector_name.read(cx).value().trim().to_owned();
        if name.is_empty() {
            self.connector_error = Some("A connection needs a name.".into());
            cx.notify();
            return;
        }
        let id = self.connector_editing.unwrap_or_else(Uuid::new_v4);
        let secret = self.connector_credential.read(cx).value().to_string();
        let credential_ref = if !secret.is_empty() {
            Some(format!("connector-{id}-{}", Uuid::new_v4()))
        } else {
            self.connectors
                .iter()
                .find(|config| config.id == id)
                .and_then(|config| config.credential_ref.clone())
        };
        let command = self.connector_command.read(cx).value().trim().to_owned();
        let config = ConnectorConfig {
            id,
            name,
            transport: self.connector_transport.clone(),
            endpoint: self.connector_endpoint.read(cx).value().trim().into(),
            command: (!command.is_empty()).then(|| PathBuf::from(command)),
            args: self
                .connector_args
                .read(cx)
                .value()
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(str::to_owned)
                .collect(),
            credential_ref,
            trusted: self.connector_trusted,
        };
        let request_id = Uuid::new_v4();
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| {
                host.save_connector_authenticated(
                    request_id,
                    config,
                    (!secret.is_empty()).then_some(secret),
                )
            });
        match result {
            Ok(()) => {
                self.connector_pending = Some(request_id);
                self.connector_pending_save = true;
                self.connector_error = None;
            }
            Err(error) => self.connector_error = Some(error.to_string()),
        }
        cx.notify();
    }
    pub(super) fn connector_mutation_result(
        &mut self,
        request_id: Uuid,
        result: Result<xylo_runtime::host::ConnectorMutationReceipt, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.connector_pending != Some(request_id) {
            return;
        }
        self.connector_pending = None;
        self.connector_signing_in = false;
        match result {
            Ok(receipt) => {
                let invalidated = receipt
                    .removed
                    .or_else(|| receipt.saved.as_ref().map(|config| config.id));
                if let Some(id) = receipt.removed {
                    self.connectors.retain(|config| config.id != id);
                }
                if let Some(config) = receipt.saved {
                    self.connectors.retain(|existing| existing.id != config.id);
                    self.connectors.push(config);
                    self.connectors
                        .sort_by(|left, right| left.name.cmp(&right.name));
                }
                self.connector_error = None;
                self.mcp_commands
                    .retain(|command| Some(command.connector_id) != invalidated);
                if self.connector_inspected.is_some_and(|id| {
                    Some(id) == invalidated || !self.connectors.iter().any(|config| config.id == id)
                }) {
                    self.connector_inspection_request = None;
                    self.connector_inspected = None;
                    self.connector_loading = false;
                    self.connector_tools.clear();
                }
                // A save, or removing the connection being edited (Remove
                // lives on its edit page), returns to the list.
                let edited_removed = self
                    .connector_editing
                    .is_some_and(|id| !self.connectors.iter().any(|config| config.id == id));
                if self.connector_pending_save || edited_removed {
                    self.clear_connector_form(window, cx);
                }
            }
            Err(error) => {
                self.connector_error = Some(format!(
                    "Connection change failed: {error}. Your form is retained; retry when ready."
                ))
            }
        }
        cx.notify();
    }
    fn edit_connector(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        if self.connector_pending.is_some() {
            return;
        }
        let Some(config) = self
            .connectors
            .iter()
            .find(|config| config.id == id)
            .cloned()
        else {
            return;
        };
        self.connector_editing = Some(id);
        self.connector_form_open = true;
        self.connector_transport = config.transport;
        self.connector_trusted = config.trusted;
        self.connector_name
            .update(cx, |input, cx| input.set_value(config.name, window, cx));
        self.connector_command.update(cx, |input, cx| {
            input.set_value(
                config
                    .command
                    .map(|path| path.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                window,
                cx,
            )
        });
        self.connector_args.update(cx, |input, cx| {
            input.set_value(config.args.join("\n"), window, cx)
        });
        self.connector_endpoint
            .update(cx, |input, cx| input.set_value(config.endpoint, window, cx));
        self.connector_credential
            .update(cx, |input, cx| input.set_value("", window, cx));
        cx.notify();
    }
    /// Opens the empty form for a new connection.
    fn open_connector_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.connector_pending.is_some() {
            return;
        }
        self.clear_connector_form(window, cx);
        self.connector_form_open = true;
        cx.notify();
    }
    /// Leaves the form for the list, discarding what was typed.
    fn close_connector_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.connector_pending.is_some() {
            return;
        }
        self.clear_connector_form(window, cx);
        cx.notify();
    }
    fn clear_connector_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.connector_form_open = false;
        self.connector_editing = None;
        self.connector_error = None;
        self.connector_transport = "stdio".into();
        self.connector_trusted = false;
        for input in [
            &self.connector_name,
            &self.connector_command,
            &self.connector_endpoint,
            &self.connector_credential,
        ] {
            input.update(cx, |input, cx| input.set_value("", window, cx));
        }
        self.connector_args
            .update(cx, |input, cx| input.set_value("", window, cx));
    }
    fn confirm_remove_connector(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self
            .connectors
            .iter()
            .find(|config| config.id == id)
            .map(|config| config.name.clone())
        else {
            return;
        };
        self.confirm_destructive(
            &format!("Remove {name}?"),
            "Chats can no longer use its tools. Its saved access token is removed.",
            "Remove connection",
            window,
            cx,
            move |view, _, cx| view.remove_connector(id, cx),
        );
    }
    fn remove_connector(&mut self, id: Uuid, cx: &mut Context<Self>) {
        if self.connector_pending.is_some() {
            return;
        }
        let request_id = Uuid::new_v4();
        match self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.remove_connector(request_id, id))
        {
            Ok(()) => {
                self.connector_pending = Some(request_id);
                self.connector_pending_save = false;
            }
            Err(error) => self.connector_error = Some(error.to_string()),
        }
        cx.notify();
    }

    pub(super) fn inspect_connector(&mut self, id: Uuid, cx: &mut Context<Self>) {
        let request_id = Uuid::new_v4();
        self.connector_inspection_request = Some(request_id);
        self.connector_inspected = Some(id);
        self.connector_loading = true;
        self.connector_error = None;
        self.connector_tools.clear();
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.connector_tools_scoped(request_id, id, self.workspace.clone()));
        if let Err(error) = result {
            self.connector_loading = false;
            self.connector_error = Some(error.to_string());
        }
        cx.notify();
    }
    /// The tools of the inspected connection, under its own section.
    fn connector_inspection_section(&self, cx: &Context<Self>) -> Option<Div> {
        let id = self.connector_inspected?;
        let name = self
            .connectors
            .iter()
            .find(|connection| connection.id == id)
            .map(|connection| connection.name.clone())
            .unwrap_or_else(|| "Connection".into());
        let status = if self.connector_loading {
            Some("Loading tools…".to_owned())
        } else if let Some(error) = &self.connector_error {
            Some(
                if error.contains("401")
                    || error.contains("403")
                    || error.contains("credential is missing")
                {
                    format!(
                        "Authentication failed. Edit this connection to add or replace its access token, save, then retry. {error}"
                    )
                } else {
                    error.clone()
                },
            )
        } else if self.connector_tools.is_empty() {
            Some("No tools returned".into())
        } else {
            None
        };
        let mut body = div().w_full().flex().flex_col().gap(px(10.));
        if self.connector_loading && self.connector_tools.is_empty() {
            body =
                body.child(settings_ui::list_container((0..3).map(|_| {
                    settings_ui::skeleton_list_item(false).into_any_element()
                })));
        }
        if let Some(status) = status {
            let failed = self.connector_error.is_some();
            body = body.child(
                div()
                    .id("connector-inspection-status")
                    .role(Role::Status)
                    .aria_label(status.clone())
                    .test_support()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .child(
                        settings_ui::caption(status)
                            .flex_1()
                            .when(failed, |text| text.text_color(rgb(danger()))),
                    )
                    .when(failed, |row| {
                        row.child(
                            settings_ui::secondary_button(
                                "retry-connector-inspection",
                                "Try again",
                            )
                            .on_click(
                                cx.listener(move |view, _, _, cx| view.inspect_connector(id, cx)),
                            ),
                        )
                    }),
            );
        }
        if !self.connector_loading
            && self.connector_error.is_none()
            && !self.connector_tools.is_empty()
        {
            let weak = cx.entity().downgrade();
            let count = self.connector_tools.len();
            body = body.child(
                settings_ui::list_frame().child(
                    uniform_list("connector-tool-results", count, move |range, _, cx| {
                        weak.update(cx, |view, _| {
                            range
                                .filter_map(|index| {
                                    let tool = view.connector_tools.get(index)?;
                                    Some(
                                        settings_ui::list_item(
                                            ("connector-tool", index),
                                            None,
                                            tool.name.clone(),
                                            Some(tool.description.clone().into()),
                                        )
                                        .divider(index > 0)
                                        .into_any_element(),
                                    )
                                })
                                .collect()
                        })
                        .unwrap_or_default()
                    })
                    .h(px((count as f32 * settings_ui::LIST_ITEM_HEIGHT)
                        .min(6.5 * settings_ui::LIST_ITEM_HEIGHT))),
                ),
            );
        }
        Some(settings_ui::section(
            format!("{name} tools"),
            Some("What chats can call through this connection.".into()),
            body,
        ))
    }
    pub(super) fn general_settings_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = settings_ui::segmented(
            "theme",
            [("system", "System"), ("light", "Light"), ("dark", "Dark")]
                .into_iter()
                .map(|(key, label)| {
                    let view = cx.entity().downgrade();
                    settings_ui::SelectOption::new(
                        label,
                        self.settings.theme == key,
                        move |window, cx| {
                            let _ = view.update(cx, |view, cx| {
                                if view.settings.theme != key {
                                    view.settings.theme = key.into();
                                    view.apply_theme(window, cx);
                                    view.persist_ui(cx);
                                }
                            });
                        },
                    )
                    .id(SharedString::from(format!("theme-{key}")))
                })
                .collect(),
        );
        let modifier = if cfg!(target_os = "macos") {
            "⌘"
        } else {
            "Ctrl"
        };
        let shortcuts = [
            ("New chat", "N"),
            ("Search chats", "K"),
            ("Toggle terminal", "J"),
            ("Save file", "S"),
        ]
        .into_iter()
        .map(|(label, key)| {
            settings_ui::setting_row(
                label,
                None,
                Some(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(4.))
                        .child(kbd(modifier))
                        .child(kbd(key))
                        .into_any_element(),
                ),
            )
            .into_any_element()
        });
        settings_ui::settings_page("general-settings", "General")
            .description("Appearance, shortcuts, responses and the version of Xylo you run.")
            .child(settings_ui::section(
                "Appearance",
                None,
                settings_ui::rows([settings_ui::setting_row(
                    "Theme",
                    Some("System follows your Mac's light or dark appearance.".into()),
                    Some(theme.into_any_element()),
                )
                .into_any_element()]),
            ))
            .child(settings_ui::section(
                "Keyboard shortcuts",
                None,
                settings_ui::rows(shortcuts),
            ))
            .child(settings_ui::section(
                "Advanced",
                None,
                settings_ui::rows([self.response_timeout_row(cx)]),
            ))
            .child(self.status_command_settings(cx))
            .child(self.about_section(cx))
            .into_any_element()
    }
    fn response_timeout_row(&self, cx: &mut Context<Self>) -> AnyElement {
        let current = self.settings.provider.timeout_seconds;
        let timeouts = settings_ui::segmented(
            "agent-timeout",
            [30u64, 120, 300]
                .into_iter()
                .map(|seconds| {
                    let view = cx.entity().downgrade();
                    settings_ui::SelectOption::new(
                        match seconds {
                            120 => "2 min".to_owned(),
                            300 => "5 min".to_owned(),
                            _ => format!("{seconds} s"),
                        },
                        current == seconds,
                        move |_, cx| {
                            let _ =
                                view.update(cx, |view, cx| view.set_response_timeout(seconds, cx));
                        },
                    )
                    .id(SharedString::from(format!("agent-timeout-{seconds}")))
                })
                .collect(),
        );
        let detail: SharedString = if [30, 120, 300].contains(&current) {
            "Time to the first response, then the longest pause allowed between streaming updates."
                .into()
        } else {
            format!("Currently {current} seconds. Time to the first response, then the longest pause between updates.")
                .into()
        };
        settings_ui::setting_row(
            "Response timeout",
            Some(detail),
            Some(timeouts.into_any_element()),
        )
        .into_any_element()
    }
    fn set_response_timeout(&mut self, seconds: u64, cx: &mut Context<Self>) {
        if self.settings.provider.timeout_seconds == seconds {
            return;
        }
        self.settings.provider.timeout_seconds = seconds;
        let provider = self.settings.provider.clone();
        self.settings.remember_model(provider);
        let settings = self.settings.clone();
        self.host_call(cx, |host| host.save_settings(settings));
        cx.notify();
    }
    pub(super) fn permission_settings_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let modes = settings_ui::radio_list(
            "permission-modes",
            PermissionMode::ALL
                .into_iter()
                .enumerate()
                .map(|(index, mode)| {
                    let view = cx.entity().downgrade();
                    settings_ui::radio_option(
                        ("permission-setting", index),
                        mode.label(),
                        mode.description(),
                        self.current_permission_mode() == mode,
                    )
                    .caution(mode == PermissionMode::FullAccess)
                    .on_choose(move |_, cx| {
                        let _ = view.update(cx, |view, cx| view.set_permission_mode(mode, cx));
                    })
                }),
        );
        let storage = settings_ui::select(
            "secret-storage",
            "Where keys are kept",
            [SecretStorage::LocalFile, SecretStorage::OsKeychain]
                .into_iter()
                .map(|option| {
                    let view = cx.entity().downgrade();
                    settings_ui::SelectOption::new(
                        secret_storage_label(option),
                        self.settings.secret_storage == option,
                        move |window, cx| {
                            let _ = view.update(cx, |view, cx| {
                                view.confirm_secret_storage(option, window, cx)
                            });
                        },
                    )
                })
                .collect(),
        );
        settings_ui::settings_page("permissions-settings", "Permissions")
            .description("How Xylo's actions are approved. Changes apply from the next action, including in running tasks.")
            .child(settings_ui::section("Approval mode", None, modes))
            .child(settings_ui::section(
                "Permission rules",
                Some("Deny takes priority over ask, then allow. Managed rules cannot be edited.".into()),
                self.approval_rules_section(cx),
            ))
            .child(settings_ui::section(
                "Worktrees",
                Some("Isolated checkouts retained by chats. Removal preserves Git branches.".into()),
                self.worktree_settings(cx),
            ))
            .child(settings_ui::section(
                "Security",
                Some("Switching removes saved keys and tokens; you'll reconnect providers and connections afterwards.".into()),
                settings_ui::rows([settings_ui::setting_row(
                    "Where keys are kept",
                    Some(secret_storage_detail(self.settings.secret_storage).into()),
                    Some(storage),
                )
                .into_any_element()]),
            ))
            .into_any_element()
    }
    fn approval_rules_section(&self, cx: &mut Context<Self>) -> Div {
        let mut section = div().w_full().flex().flex_col().gap(px(10.));
        let mut destination = div().flex().gap_2();
        for (project, label) in [(false, "Local settings"), (true, "Shared project settings")] {
            destination = destination.child(
                settings_ui::secondary_button(
                    if project {
                        "permission-destination-project"
                    } else {
                        "permission-destination-local"
                    },
                    label,
                )
                .disabled(self.settings.permission_rules_project == project)
                .on_click(cx.listener(move |view, _, _, cx| {
                    view.settings.permission_rules_project = project;
                    let settings = view.settings.clone();
                    view.host_call(cx, |host| host.save_settings(settings));
                    cx.notify();
                })),
            );
        }
        section = section.child(destination).child(subtitle(
            "New rules and Always allow use this destination. Local settings are git-ignored.",
        ));
        section = section.child(Input::new(&self.permission_pattern));
        let mut actions = div().flex().gap_2();
        for decision in [
            PermissionDecision::Allow,
            PermissionDecision::Ask,
            PermissionDecision::Deny,
        ] {
            actions = actions.child(
                settings_ui::secondary_button(
                    SharedString::from(format!("permission-add-{}", decision.key())),
                    format!("Add {} rule", decision.key()),
                )
                .disabled(self.workspace.is_none() || self.permissions.loading)
                .on_click(cx.listener(move |view, _, _, cx| {
                    let Some(workspace) = view.workspace.clone() else {
                        return;
                    };
                    let pattern = view.permission_pattern.read(cx).value().trim().to_owned();
                    let project = view.settings.permission_rules_project;
                    view.host_call(cx, |host| {
                        host.save_permission_rule(workspace, pattern, decision, project)
                    });
                    view.permissions.loading = true;
                    cx.notify();
                })),
            );
        }
        section = section.child(actions).child(
            settings_ui::secondary_button("refresh-permission-files", "Reload rules")
                .disabled(self.permissions.loading)
                .on_click(cx.listener(|view, _, _, cx| view.refresh_approval_rules(cx))),
        );

        if let Some(error) = &self.permissions.error {
            section = section.child(settings_ui::inline_error(error));
        }
        if self.workspace.is_none() {
            return section.child(settings_ui::empty_state(
                AppIcon::Permissions,
                "No project open",
                "Open a project to see the actions you always allow in it.",
                None,
            ));
        }
        if self.permissions.workspace != self.workspace {
            // The project changed while this page was open.
            return section.child(settings_ui::rows([settings_ui::setting_row(
                "Another project is open",
                Some("Its always-allowed actions have not been loaded yet.".into()),
                Some(
                    settings_ui::secondary_button(
                        "load-approval-rules",
                        "Show this project's actions",
                    )
                    .on_click(cx.listener(|view, _, _, cx| view.refresh_approval_rules(cx)))
                    .into_any_element(),
                ),
            )
            .into_any_element()]));
        }
        if self.permissions.rules.is_empty() {
            if self.permissions.loading {
                return section.child(settings_ui::loading_list(3, self.permissions.loading_since));
            }
            return section.child(settings_ui::empty_state(
                AppIcon::Permissions,
                "No permission rules",
                "Add a rule above, edit a settings file, or choose Always allow on an approval.",
                None,
            ));
        }
        let rules =
            self.permissions
                .rules
                .iter()
                .enumerate()
                .map(|(index, rule)| {
                    let key = rule.clone();
                    settings_ui::list_item(
                        ("approval-rule", index),
                        None,
                        format!("{}: {}", rule.decision.key(), rule.pattern),
                        Some(rule.source.display().to_string().into()),
                    )
                    .trailing(
                        settings_ui::text_button(
                            ("revoke-approval-rule", index),
                            if rule.managed { "Managed" } else { "Remove" },
                        )
                        .disabled(self.permissions.loading || rule.managed)
                        .on_click(cx.listener(move |view, _, _, cx| {
                            view.revoke_approval_rule(key.clone(), cx)
                        })),
                    )
                    .into_any_element()
                });
        section.child(settings_ui::list_container(rules))
    }

    fn connector_form(&self, cx: &mut Context<Self>) -> Div {
        let busy = self.connector_pending.is_some();
        let transport = settings_ui::segmented(
            "transport",
            [
                ("stdio", "Local command"),
                ("http", "Remote server"),
                ("sse", "Legacy SSE"),
            ]
            .into_iter()
            .map(|(value, label)| {
                let view = cx.entity().downgrade();
                settings_ui::SelectOption::new(
                    label,
                    self.connector_transport == value,
                    move |_, cx| {
                        let _ = view.update(cx, |view, cx| {
                            if view.connector_pending.is_none() {
                                view.connector_transport = value.into();
                                cx.notify();
                            }
                        });
                    },
                )
                .id(SharedString::from(format!("transport-{value}")))
            })
            .collect(),
        );
        let mut fields = vec![
            settings_ui::field("Type", transport, None).into_any_element(),
            settings_ui::field(
                "Connection name",
                Input::new(&self.connector_name)
                    .readonly(busy)
                    .aria_label("Connection name"),
                None,
            )
            .into_any_element(),
        ];
        if self.connector_transport == "stdio" {
            fields.push(
                settings_ui::field(
                    "Server command",
                    Input::new(&self.connector_command)
                        .readonly(busy)
                        .aria_label("Server command"),
                    Some("Path to an installed server executable, for example /absolute/path/to/server. Nothing runs when you save.".into()),
                )
                .into_any_element(),
            );
            fields.push(
                settings_ui::field(
                    "Arguments, one per line",
                    Textarea::new(&self.connector_args)
                        .readonly(busy)
                        .aria_label("Arguments, one per line"),
                    Some("Optional. Each argument on its own line, for example --workspace then /absolute/path/to/project. These are arguments, not a shell command.".into()),
                )
                .into_any_element(),
            );
        } else {
            fields.push(
                settings_ui::field(
                    "Server endpoint",
                    Input::new(&self.connector_endpoint)
                        .readonly(busy)
                        .aria_label("Server endpoint"),
                    Some("The endpoint from your server administrator, for example https://tools.example.com/mcp. Saving does not contact it.".into()),
                )
                .into_any_element(),
            );
            fields.push(
                settings_ui::field(
                    "Access token (optional)",
                    Input::new(&self.connector_credential)
                        .readonly(busy)
                        .aria_label("Access token (optional)"),
                    Some("Kept in Xylo's secret storage. Leave blank when editing to keep the saved token.".into()),
                )
                .into_any_element(),
            );
        }
        fields.push(
            settings_ui::setting_row(
                "Trust read-only hints",
                Some("Let read-only tools from this server run without asking and in parallel. Only turn on for servers you control.".into()),
                Some(
                    settings_ui::switch(
                        "connector-trusted",
                        "Trust read-only hints",
                        self.connector_trusted,
                    )
                    .disabled(busy)
                    .on_change(cx.listener(|view, checked, _, cx| {
                        if view.connector_pending.is_none() {
                            view.connector_trusted = *checked;
                            cx.notify();
                        }
                    }))
                    .into_any_element(),
                ),
            )
            .into_any_element(),
        );
        fields.push(
            settings_ui::form_actions([
                settings_ui::text_button("connector-cancel", "Cancel")
                    .disabled(busy)
                    .on_click(
                        cx.listener(|view, _, window, cx| view.close_connector_form(window, cx)),
                    )
                    .into_any_element(),
                settings_ui::busy(
                    settings_ui::primary_button(
                        "connector-save",
                        if self.connector_editing.is_some() {
                            "Save connection"
                        } else {
                            "Add connection"
                        },
                    ),
                    busy,
                )
                .on_click(cx.listener(|view, _, _, cx| view.save_connector(cx)))
                .into_any_element(),
            ])
            .into_any_element(),
        );
        settings_ui::form_column(fields)
    }
    /// A saved connection; the row opens its edit page.
    fn connection_item(&self, index: usize, connection: &ConnectorConfig) -> settings_ui::ListItem {
        let kind = if connection.transport != "stdio" {
            "Remote server"
        } else {
            "Local command"
        };
        let target = if connection.transport != "stdio" {
            connection.endpoint.clone()
        } else {
            connection
                .command
                .as_ref()
                .map(|command| command.to_string_lossy().into_owned())
                .unwrap_or_default()
        };
        let subtitle: SharedString = if target.is_empty() {
            kind.into()
        } else {
            format!("{kind} · {target}").into()
        };
        let id_text = connection.id.to_string();
        let item = settings_ui::list_item(
            ("connector-edit", index),
            Some(settings_ui::monogram(&connection.name, &id_text).into_any_element()),
            connection.name.clone(),
            Some(subtitle),
        );
        if connection.trusted {
            item.title_tag(TRUSTED_TAG, settings_ui::Tone::Success)
        } else {
            item
        }
    }

    /// Connections: the saved list with Add connection, or the add/edit form
    /// as a sub-page once Add or a row is chosen.
    pub(super) fn connections_settings_page(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.connector_form_open || self.connector_editing.is_some() {
            return self.connection_form_page(cx);
        }
        let busy = self.connector_pending.is_some();
        let add = || {
            settings_ui::primary_button("connector-add", "Add connection")
                .disabled(busy)
                .on_click(cx.listener(|view, _, window, cx| view.open_connector_form(window, cx)))
        };
        let mut page = settings_ui::settings_page("connections-settings", "Connections")
            .description("Servers that give Xylo extra tools through the Model Context Protocol.");
        if !self.connectors.is_empty() {
            page = page.action(add());
        }
        if let Some(error) = &self.connector_error
            && self.connector_inspected.is_none()
        {
            page = page.child(settings_ui::inline_error(error));
        }
        if self.connector_signing_in
            && let Some(request) = self.connector_pending
        {
            page = page.child(
                settings_ui::text_button("connector-cancel-sign-in", "Cancel sign-in").on_click(
                    cx.listener(move |view, _, _, cx| {
                        if let Some(host) = &view.host
                            && let Err(error) = host.cancel_capability(request)
                        {
                            view.connector_error = Some(error.to_string());
                        }
                        cx.notify();
                    }),
                ),
            );
        }
        let list = if self.connectors.is_empty() {
            settings_ui::empty_state(
                AppIcon::Connectors,
                "No connections yet",
                "Connect a local command or a remote server, then inspect its tools.",
                Some(add().into_any_element()),
            )
        } else {
            settings_ui::list_container(self.connectors.iter().enumerate().map(
                |(index, connection)| {
                    let id = connection.id;
                    let mut actions = div().flex().gap_2();
                    if connection.transport != "stdio" {
                        actions = actions.child(
                            settings_ui::text_button(("connector-sign-in", index), "Sign in")
                                .disabled(busy)
                                .on_click(cx.listener(move |view, _, _, cx| {
                                    let request = Uuid::new_v4();
                                    match view
                                        .host
                                        .as_ref()
                                        .context("Application service unavailable")
                                        .and_then(|host| host.connector_sign_in(request, id, None))
                                    {
                                        Ok(()) => {
                                            view.connector_pending = Some(request);
                                            view.connector_signing_in = true;
                                            view.connector_pending_save = false;
                                            view.connector_error = None;
                                        }
                                        Err(error) => {
                                            view.connector_error = Some(error.to_string())
                                        }
                                    }
                                    cx.notify();
                                })),
                        );
                    }
                    self.connection_item(index, connection)
                        .trailing(
                            actions.child(
                                settings_ui::busy(
                                    settings_ui::text_button(
                                        ("connector-tools", index),
                                        "Inspect tools",
                                    ),
                                    self.connector_loading && self.connector_inspected == Some(id),
                                )
                                .on_click(
                                    cx.listener(move |view, _, _, cx| {
                                        view.inspect_connector(id, cx)
                                    }),
                                ),
                            ),
                        )
                        .on_click(cx.listener(move |view, _, window, cx| {
                            view.edit_connector(id, window, cx)
                        }))
                        .into_any_element()
                },
            ))
        };
        // An empty list is the empty state alone; a heading over it adds nothing.
        page = if self.connectors.is_empty() {
            page.child(list)
        } else {
            page.child(settings_ui::section("Your connections", None, list))
        };
        if let Some(inspection) = self.connector_inspection_section(cx) {
            page = page.child(inspection);
        }
        if !self.mcp_commands.is_empty() {
            page = page.child(settings_ui::section(
                "MCP commands",
                Some("Choose a prompt to put its command in the chat composer.".into()),
                settings_ui::list_container(self.mcp_commands.iter().enumerate().map(
                    |(index, command)| {
                        let command = command.command.clone();
                        settings_ui::list_item(("mcp-command", index), None, command.clone(), None)
                            .on_click(cx.listener(move |view, _, window, cx| {
                                if view.composer.read(cx).value().is_empty() {
                                    view.composer.update(cx, |input, cx| {
                                        input.set_value(command.clone(), window, cx);
                                        input.focus(window, cx);
                                    });
                                    view.draft_changed(cx);
                                    view.navigate(Page::Chat, cx);
                                }
                            }))
                            .into_any_element()
                    },
                )),
            ));
        }
        page.child(self.web_search_settings_section(cx))
            .into_any_element()
    }
    fn connection_form_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let editing = self.connector_editing.and_then(|id| {
            self.connectors
                .iter()
                .position(|config| config.id == id)
                .map(|index| (index, id, self.connectors[index].name.clone()))
        });
        let title: SharedString = editing.as_ref().map_or_else(
            || "Add connection".into(),
            |(_, _, name)| name.clone().into(),
        );
        let busy = self.connector_pending.is_some();
        let mut page = settings_ui::settings_page("connections-settings", title.clone())
            .breadcrumb(settings_ui::breadcrumb(
                [settings_ui::crumb("connector-back", "Connections")
                    .disabled(busy)
                    .on_click(
                        cx.listener(|view, _, window, cx| view.close_connector_form(window, cx)),
                    )],
                title,
            ));
        if let Some(error) = &self.connector_error {
            page = page.child(settings_ui::inline_error(error));
        }
        page = page.child(settings_ui::section(
            "Connection",
            Some("Save, then choose Inspect tools to discover what it offers. Saving does not start the server or check that it is reachable.".into()),
            self.connector_form(cx),
        ));
        if let Some((index, id, _)) = editing {
            page = page.child(settings_ui::danger_zone([settings_ui::setting_row(
                "Remove connection",
                Some(
                    "Chats can no longer use its tools. Its saved access token is removed.".into(),
                ),
                Some(
                    settings_ui::destructive_button(("connector-remove", index), "Remove…")
                        .disabled(busy)
                        .on_click(cx.listener(move |view, _, window, cx| {
                            view.confirm_remove_connector(id, window, cx)
                        }))
                        .into_any_element(),
                ),
            )
            .inset()
            .into_any_element()]));
        }
        page.into_any_element()
    }
}

#[cfg(test)]
#[path = "settings/tests.rs"]
mod tests;
