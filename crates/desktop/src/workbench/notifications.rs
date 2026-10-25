//! Product notification preferences. Only deliberate controls enqueue permission
//! or test actions; rendering/status refreshes cannot request OS authorization.
use super::*;
use gpui_kit::component::{Disableable, switch::Switch};
use serde_json::{Value, json};
use workbench_runtime::notifications::Preferences;

#[derive(Default)]
pub(super) struct NotificationUi {
    pub(super) status: Value,
    pub(super) pending: BTreeMap<Uuid, String>,
    pub(super) ignored: BTreeSet<Uuid>,
    pub(super) sequence: u64,
    pub(super) notice: String,
}
impl NotificationUi {
    fn preferences(&self) -> Preferences {
        serde_json::from_value(self.status["preferences"].clone()).unwrap_or_default()
    }
    fn busy(&self) -> bool {
        self.pending
            .values()
            .any(|name| name != "notifications_status")
    }
    fn saving(&self) -> bool {
        self.pending
            .values()
            .any(|name| name == "notifications_preferences")
    }
}
#[derive(Clone, Copy)]
pub(super) enum NotificationPreference {
    Enabled,
    Completion,
    Approval,
    Failure,
    Schedule,
    Background,
}

impl Workbench {
    fn notification_request(&mut self, name: &str, args: Value, cx: &mut Context<Self>) {
        if self.quitting
            || self
                .notifications
                .pending
                .values()
                .any(|pending| pending == name)
        {
            return;
        }
        let id = Uuid::new_v4();
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.capability(id, name.into(), args));
        if result.is_ok() {
            self.notifications.pending.insert(id, name.into());
            self.notifications.notice = match name {
                "notifications_request_permission" => {
                    "Waiting for your choice in the system dialog…"
                }
                "notifications_test" => "Sending test notification…",
                "notifications_preferences" => "Saving notification preferences…",
                "notifications_refresh" => "Checking system permission…",
                _ => &self.notifications.notice,
            }
            .into();
        }
        self.result(result, cx);
    }
    pub(super) fn refresh_notifications(&mut self, cx: &mut Context<Self>) {
        self.notification_request("notifications_status", json!({}), cx);
    }
    pub(super) fn set_notification_preference(
        &mut self,
        key: NotificationPreference,
        checked: bool,
        cx: &mut Context<Self>,
    ) {
        if self.notifications.status["loaded"] != true || self.notifications.saving() {
            return;
        }
        let mut preferences = self.notifications.preferences();
        match key {
            NotificationPreference::Enabled => preferences.enabled = checked,
            NotificationPreference::Completion => preferences.completion = checked,
            NotificationPreference::Approval => preferences.approval = checked,
            NotificationPreference::Failure => preferences.failure = checked,
            NotificationPreference::Schedule => preferences.schedule = checked,
            NotificationPreference::Background => preferences.background_only = checked,
        }
        self.notification_request(
            "notifications_preferences",
            json!({"preferences":preferences}),
            cx,
        );
    }
    fn cancel_notification_action(&mut self, cx: &mut Context<Self>) {
        let ids: Vec<_> = self
            .notifications
            .pending
            .iter()
            .filter(|(_, name)| {
                matches!(
                    name.as_str(),
                    "notifications_request_permission" | "notifications_test"
                )
            })
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            if let Some(host) = &self.host {
                let _ = host.cancel_capability(id);
            }
            self.notifications.pending.remove(&id);
            self.notifications.ignored.insert(id);
        }
        while self.notifications.ignored.len() > 64 {
            self.notifications.ignored.pop_first();
        }
        self.notifications.notice =
            "Request cancelled. You can close any open system dialog.".into();
        cx.notify();
    }
    pub(super) fn notification_error(
        &mut self,
        id: Uuid,
        message: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.notifications.ignored.remove(&id) {
            return true;
        }
        let Some(name) = self.notifications.pending.remove(&id) else {
            return false;
        };
        self.notifications.notice = message.into();
        if name != "notifications_status" {
            self.refresh_notifications(cx);
        }
        cx.notify();
        true
    }
    pub(super) fn notification_result(
        &mut self,
        id: Uuid,
        value: &Value,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.notifications.ignored.remove(&id) {
            self.refresh_notifications(cx);
            return true;
        }
        let Some(name) = self.notifications.pending.remove(&id) else {
            return false;
        };
        let snapshot = value.get("snapshot").unwrap_or(value);
        if snapshot["sequence"]
            .as_u64()
            .is_some_and(|sequence| sequence < self.notifications.sequence)
        {
            cx.notify();
            return true;
        }
        let accepted = snapshot["sequence"]
            .as_u64()
            .filter(|sequence| *sequence >= self.notifications.sequence);
        if let Some(sequence) = accepted {
            self.notifications.sequence = sequence;
            self.notifications.status = snapshot.clone();
            if let Some(message) = snapshot["message"].as_str() {
                self.notifications.notice = message.into();
            } else if name != "notifications_status" {
                self.notifications.notice = match name.as_str() {
                    "notifications_preferences" => "Notification preferences saved.",
                    "notifications_request_permission" | "notifications_refresh" => {
                        permission_text(snapshot["permission"].as_str())
                    }
                    _ => "",
                }
                .into();
            }
        }
        if name == "notifications_test" {
            let outcome = value.get("outcome").unwrap_or(value);
            // Native acceptance does not mean a banner bypassed Focus modes or
            // that an OS notification was actually displayed to the person.
            self.notifications.notice=match outcome.as_str() {
                Some("submitted")=>"Test sent to your notification center. System Focus settings may keep it quiet.",
                Some("cancelled")=>"Test notification cancelled.",
                _=>"No notification was sent. Check your notification preferences.",
            }.into();
        } else if name == "notifications_preferences"
            && accepted.is_some()
            && self.notifications.preferences().enabled
        {
            // A read-only permission query never calls requestAuthorization.
            self.notification_request("notifications_refresh", json!({}), cx);
        }
        cx.notify();
        true
    }
    pub(super) fn notifications_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let preferences = self.notifications.preferences();
        let loaded = self.notifications.status["loaded"] == true;
        let busy = self.notifications.busy();
        let permission = self.notifications.status["permission"].as_str();
        let deliverable = matches!(permission, Some("allowed" | "quiet" | "not_required"));
        let mut general = card()
            .max_w(px(820.))
            .child(
                Switch::new("notifications-enabled")
                    .label("Enable desktop notifications")
                    .checked(preferences.enabled)
                    .disabled(!loaded || self.notifications.saving())
                    .on_change(cx.listener(|view, checked, _, cx| {
                        view.set_notification_preference(
                            NotificationPreference::Enabled,
                            *checked,
                            cx,
                        )
                    })),
            )
            .child(subtitle(
                "Notifications are silent and contain no conversation text.",
            ));
        general = general.child(subtitle(if !loaded {
            "Loading notification preferences…"
        } else {
            permission_text(permission)
        }));
        let mut actions = div().flex().flex_wrap().gap_2();
        if permission == Some("not_determined")
            && self.notifications.status["capabilities"]["request_permission"] == true
        {
            actions = actions.child(
                primary("notifications-allow", "Allow notifications")
                    .disabled(busy || !preferences.enabled)
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.notification_request("notifications_request_permission", json!({}), cx)
                    })),
            );
        }
        actions = actions
            .child(
                button("notifications-refresh", "Refresh permission")
                    .disabled(busy || !loaded)
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.notification_request("notifications_refresh", json!({}), cx)
                    })),
            )
            .child(
                button("notifications-test", "Send test notification")
                    .disabled(busy || !preferences.enabled || !deliverable)
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.notification_request("notifications_test", json!({}), cx)
                    })),
            );
        if self.notifications.pending.values().any(|name| {
            matches!(
                name.as_str(),
                "notifications_request_permission" | "notifications_test"
            )
        }) {
            actions = actions.child(
                button("notifications-cancel", "Cancel request")
                    .on_click(cx.listener(|view, _, _, cx| view.cancel_notification_action(cx))),
            );
        }
        general = general.child(actions);
        if self.notifications.status["capabilities"]["live_activation"] == true && deliverable {
            general = general.child(subtitle(
                "Open a task from its notification while Xylo is running.",
            ));
        }
        let mut choices = card().max_w(px(820.));
        for (id, label, key, checked, description) in [
            (
                "notifications-completion",
                "Task completion",
                NotificationPreference::Completion,
                preferences.completion,
                "When a task finishes.",
            ),
            (
                "notifications-approval",
                "Approval requests",
                NotificationPreference::Approval,
                preferences.approval,
                "When a task needs your approval to continue.",
            ),
            (
                "notifications-failure",
                "Tasks needing attention",
                NotificationPreference::Failure,
                preferences.failure,
                "When a task cannot finish.",
            ),
            (
                "notifications-schedule",
                "Scheduled tasks",
                NotificationPreference::Schedule,
                preferences.schedule,
                "When a scheduled task finishes or needs attention.",
            ),
            (
                "notifications-background",
                "Only when Xylo is in the background",
                NotificationPreference::Background,
                preferences.background_only,
                "Keep desktop alerts quiet while you are using Xylo.",
            ),
        ] {
            choices = choices.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .py_1()
                    .child(
                        Switch::new(id)
                            .label(label)
                            .checked(checked)
                            .disabled(
                                !loaded || !preferences.enabled || self.notifications.saving(),
                            )
                            .on_change(cx.listener(move |view, checked, _, cx| {
                                view.set_notification_preference(key, *checked, cx)
                            })),
                    )
                    .child(subtitle(description)),
            );
        }
        div()
            .id("notifications-page")
            .tab_group()
            .size_full()
            .overflow_y_scroll()
            .p_6()
            .flex()
            .flex_col()
            .gap_4()
            .child(heading(
                "Notifications",
                "Choose when Xylo should get your attention.",
            ))
            .child(general)
            .child(choices)
            .child(subtitle(self.notifications.notice.clone()))
            .into_any_element()
    }
}
fn permission_text(permission: Option<&str>) -> &'static str {
    match permission {
        Some("allowed") => "Notifications are allowed.",
        Some("quiet") => "Your system delivers notifications quietly.",
        Some("not_required") => "Your desktop notification center is ready.",
        Some("not_determined") => "Choose Allow notifications to give Xylo permission.",
        Some("denied") => {
            "Notifications are blocked. Allow Xylo in your system notification settings, then refresh."
        }
        Some("unsupported") => "Native notifications are unavailable in this installation.",
        Some("unavailable") => {
            "Your notification center is unavailable. Check your desktop settings and refresh."
        }
        _ => "Refresh permission to check your system notification settings.",
    }
}
