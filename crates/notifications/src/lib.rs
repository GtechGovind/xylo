//! Opt-in native notifications. Host lifecycle events contain IDs/enums only;
//! fixed product copy is selected here, never model/tool-produced content.
pub mod platform;
mod store;
mod types;
pub use types::*;

use anyhow::{Context, Result, ensure};
use async_channel::Receiver;
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Mutex, OnceCell};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[async_trait]
pub trait Adapter: Send + Sync {
    fn capabilities(&self) -> Capabilities;
    async fn permission(&self, cancel: CancellationToken) -> Result<Permission>;
    async fn request_permission(&self, cancel: CancellationToken) -> Result<Permission>;
    async fn post(&self, notice: NativeNotice, cancel: CancellationToken) -> Result<()>;
    async fn dismiss(&self, tag: String, cancel: CancellationToken) -> Result<()>;
}

#[derive(Clone)]
pub struct Service {
    store: store::Store,
    adapter: Arc<dyn Adapter>,
    incoming: Receiver<String>,
    state: Arc<std::sync::Mutex<Snapshot>>,
    loaded: Arc<OnceCell<()>>,
    operation: Arc<Mutex<()>>,
    preferences_operation: Arc<Mutex<()>>,
    journal: Arc<AtomicBool>,
    policy_epoch: Arc<AtomicU64>,
}
impl Service {
    /// Creates no permission dialog, OS notification, database or polling task.
    pub fn new(data: PathBuf, identity: AppIdentity) -> Result<Self> {
        ensure!(
            !identity.id.is_empty()
                && identity.id.len() <= 128
                && identity
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-')),
            "Invalid application identity"
        );
        let (adapter, incoming) = platform::native(identity)?;
        Ok(Self::with_adapter(data, adapter, incoming))
    }
    fn with_adapter(data: PathBuf, adapter: Arc<dyn Adapter>, incoming: Receiver<String>) -> Self {
        Self {
            store: store::Store::new(&data),
            state: Arc::new(std::sync::Mutex::new(Snapshot {
                sequence: 0,
                loaded: false,
                preferences: Preferences::default(),
                permission: Permission::Unknown,
                capabilities: adapter.capabilities(),
                message: None,
            })),
            adapter,
            incoming,
            loaded: Arc::new(OnceCell::new()),
            operation: Arc::new(Mutex::new(())),
            preferences_operation: Arc::new(Mutex::new(())),
            journal: Arc::new(AtomicBool::new(false)),
            policy_epoch: Arc::new(AtomicU64::new(0)),
        }
    }
    /// Memory only. Native and SQLite work always run away from GPUI rendering.
    pub fn status(&self) -> Snapshot {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.sequence = state.sequence.saturating_add(1);
        state.clone()
    }
    pub async fn initialize(&self) -> Result<Snapshot> {
        self.loaded
            .get_or_try_init(|| async {
                let store = self.store.clone();
                let (preferences, exists) = tokio::task::spawn_blocking(move || {
                    Ok::<_, anyhow::Error>((store.preferences()?, store.exists()))
                })
                .await??;
                self.journal.store(exists, Ordering::Release);
                let enabled = preferences.enabled;
                {
                    let mut state = self
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    state.preferences = preferences;
                    state.loaded = true;
                }
                if enabled {
                    // Read-only registration/settings refresh restores the live
                    // activation delegate after restart. It never asks permission.
                    match self.adapter.permission(CancellationToken::new()).await {
                        Ok(permission) => self.update_permission(permission),
                        Err(error) => {
                            tracing::debug!(%error,"Refresh notification settings");
                            self.update_permission(Permission::Unavailable);
                        }
                    }
                }
                Ok::<_, anyhow::Error>(())
            })
            .await?;
        Ok(self.status())
    }
    pub async fn set_preferences(&self, preferences: Preferences) -> Result<Snapshot> {
        // An accepted settings write finishes even if its UI waiter is dropped.
        // This lock is separate from native delivery/permission callbacks, so
        // turning notifications off never waits for a permission dialog.
        let service = self.clone();
        tokio::spawn(async move { service.set_preferences_inner(preferences).await })
            .await
            .context("Notification settings task stopped")?
    }
    async fn set_preferences_inner(&self, preferences: Preferences) -> Result<Snapshot> {
        self.initialize().await?;
        let _settings = self.preferences_operation.lock().await;
        self.policy_epoch.fetch_add(1, Ordering::AcqRel);
        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Tightening policy is fail-closed before touching disk. Enabling
            // only becomes effective after the explicit preference is durable.
            state.preferences.enabled &= preferences.enabled;
            state.preferences.completion &= preferences.completion;
            state.preferences.approval &= preferences.approval;
            state.preferences.failure &= preferences.failure;
            state.preferences.schedule &= preferences.schedule;
            state.preferences.background_only |= preferences.background_only;
        }
        let store = self.store.clone();
        let saved = preferences.clone();
        tokio::task::spawn_blocking(move||store.save_preferences(&saved)).await?
            .context("Could not save notification settings; delivery remains restricted for this session")?;
        self.journal.store(true, Ordering::Release);
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .preferences = preferences.clone();
        if !preferences.enabled {
            let store = self.store.clone();
            let tags = tokio::task::spawn_blocking(move || store.invalidate_all()).await??;
            for tag in tags {
                let _ = self.adapter.dismiss(tag, CancellationToken::new()).await;
            }
        }
        Ok(self.status())
    }
    fn update_permission(&self, permission: Permission) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.permission = permission;
        state.capabilities = self.adapter.capabilities();
        state.message = match permission {
            Permission::Denied => Some("Notifications are blocked in your system settings.".into()),
            Permission::Unsupported => {
                Some("Native notifications are not supported by this installation.".into())
            }
            Permission::Unavailable => Some(
                "Your notification center is unavailable. Check desktop settings and try again."
                    .into(),
            ),
            _ => None,
        };
    }
    pub async fn refresh_permission(&self, cancel: CancellationToken) -> Result<Snapshot> {
        self.initialize().await?;
        let permission = self.adapter.permission(cancel).await?;
        self.update_permission(permission);
        Ok(self.status())
    }
    /// Only a trusted explicit native UI action may call this. observe() never
    /// requests permission or enables preferences, even if an agent asks it to.
    pub async fn request_permission(
        &self,
        action: UserAction,
        cancel: CancellationToken,
    ) -> Result<Snapshot> {
        ensure!(
            matches!(action, UserAction::RequestPermission),
            "Choose Allow notifications to request permission"
        );
        self.initialize().await?;
        let _operation = self.operation.lock().await;
        ensure!(
            self.status().preferences.enabled,
            "Enable notifications first"
        );
        let permission = self.adapter.request_permission(cancel).await?;
        self.update_permission(permission);
        Ok(self.status())
    }
    pub async fn observe(&self, event: NoticeEvent, cancel: CancellationToken) -> Result<Outcome> {
        let cancel = cancel.child_token();
        let guard = cancel.clone().drop_guard();
        let service = self.clone();
        let result = tokio::spawn(async move { service.observe_inner(event, cancel).await })
            .await
            .context("Notification delivery task stopped")?;
        guard.disarm();
        result
    }
    async fn observe_inner(
        &self,
        event: NoticeEvent,
        cancel: CancellationToken,
    ) -> Result<Outcome> {
        self.initialize().await?;
        let _operation = self.operation.lock().await;
        if cancel.is_cancelled() {
            return Ok(Outcome::Cancelled);
        }
        if !self.status().preferences.enabled && !self.journal.load(Ordering::Acquire) {
            return Ok(Outcome::Suppressed);
        }
        let at = now()?;
        // Recovered history must keep its original timestamp; do not announce
        // old tasks after restart. Clock-skew tolerance is five seconds.
        if event.occurred_at_ms > at.saturating_add(5000)
            || event.occurred_at_ms < at.saturating_sub(120000)
        {
            return Ok(Outcome::Stale);
        }
        let tag = tag(&event);
        let store = self.store.clone();
        let record = event.clone();
        let key = tag.clone();
        let claim = tokio::task::spawn_blocking(move || store.claim(&record, &key, at)).await??;
        for previous in claim.retract {
            let _ = self.adapter.dismiss(previous, cancel.clone()).await;
        }
        if !claim.accepted {
            return Ok(Outcome::Duplicate);
        }
        let Some((title, body)) = event.kind.text() else {
            return Ok(Outcome::Suppressed);
        };
        let preferences = self.status().preferences;
        if !preferences.enabled
            || !event.kind.enabled(&preferences)
            || (preferences.background_only && !event.background)
        {
            return Ok(Outcome::Suppressed);
        }
        let permission = self.adapter.permission(cancel.clone()).await?;
        self.update_permission(permission);
        if !permission.allows_delivery() {
            return Ok(Outcome::Suppressed);
        }
        if !self.permits(&event) || cancel.is_cancelled() {
            return Ok(Outcome::Cancelled);
        }
        let epoch = self.policy_epoch.load(Ordering::Acquire);
        let notice = NativeNotice {
            tag: tag.clone(),
            title,
            body,
        };
        let result = self.adapter.post(notice, cancel.clone()).await;
        if cancel.is_cancelled()
            || !self.permits(&event)
            || epoch != self.policy_epoch.load(Ordering::Acquire)
        {
            let _ = self
                .adapter
                .dismiss(tag.clone(), CancellationToken::new())
                .await;
            let store = self.store.clone();
            tokio::task::spawn_blocking(move || store.invalidate(&tag)).await??;
            return Ok(Outcome::Cancelled);
        }
        result?;
        Ok(Outcome::Submitted)
    }
    fn permits(&self, event: &NoticeEvent) -> bool {
        let preferences = self.status().preferences;
        preferences.enabled
            && event.kind.enabled(&preferences)
            && (!preferences.background_only || event.background)
    }
    pub async fn test_notification(
        &self,
        action: UserAction,
        cancel: CancellationToken,
    ) -> Result<Outcome> {
        ensure!(
            matches!(action, UserAction::SendTest),
            "Choose Send test notification to continue"
        );
        let cancel = cancel.child_token();
        let guard = cancel.clone().drop_guard();
        let service = self.clone();
        let result = tokio::spawn(async move { service.test_inner(cancel).await })
            .await
            .context("Notification test task stopped")?;
        guard.disarm();
        result
    }
    async fn test_inner(&self, cancel: CancellationToken) -> Result<Outcome> {
        self.initialize().await?;
        let _operation = self.operation.lock().await;
        ensure!(
            self.status().preferences.enabled,
            "Enable notifications first"
        );
        let permission = self.adapter.permission(cancel.clone()).await?;
        self.update_permission(permission);
        ensure!(
            permission.allows_delivery(),
            "Allow notifications in your system settings first"
        );
        ensure!(!cancel.is_cancelled(), "Notification test cancelled");
        let epoch = self.policy_epoch.load(Ordering::Acquire);
        let tag = format!("xylo-test-{}", Uuid::new_v4().simple());
        let result = self
            .adapter
            .post(
                NativeNotice {
                    tag: tag.clone(),
                    title: "Xylo notifications",
                    body: "Notifications are ready. Open Xylo to continue.",
                },
                cancel.clone(),
            )
            .await;
        if cancel.is_cancelled()
            || !self.status().preferences.enabled
            || epoch != self.policy_epoch.load(Ordering::Acquire)
        {
            let _ = self.adapter.dismiss(tag, CancellationToken::new()).await;
            return Ok(Outcome::Cancelled);
        }
        result?;
        Ok(Outcome::Submitted)
    }
    /// The adapter supplies real native activation tags. Validate/consume the
    /// durable route once, then let the app activate its own existing window.
    pub async fn next_activation(&self, cancel: CancellationToken) -> Result<Option<Activation>> {
        self.initialize().await?;
        loop {
            let tag = tokio::select! {_ = cancel.cancelled()=>return Ok(None),tag=self.incoming.recv()=>match tag{Ok(tag)=>tag,Err(_)=>return Ok(None)}};
            if !self.status().preferences.enabled {
                continue;
            }
            let store = self.store.clone();
            let at = now()?;
            if let Some(activation) =
                tokio::task::spawn_blocking(move || store.activate(&tag, at)).await??
            {
                return Ok(Some(activation));
            }
        }
    }
}
fn tag(event: &NoticeEvent) -> String {
    let value = format!(
        "{}:{}:{}:{}",
        event.session_id,
        event.turn_id,
        event.event_id,
        event.kind.key()
    );
    format!("wb-{:x}", Sha256::digest(value.as_bytes()))[..35].to_owned()
}
pub fn now() -> Result<i64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis()
        .try_into()
        .context("Clock is outside the supported range")
}

#[cfg(test)]
mod tests;
