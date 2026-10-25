//! Product-facing update lifecycle. Never stores trust material in UI values.
//! The host owns admission/quiescence; this service only performs explicit
//! Check, Download, and Install actions, each on the blocking pool.
use anyhow::{Context, Result, ensure};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc};
use tokio_util::sync::CancellationToken;
use workbench_distribution::{StagedUpdate, UpdateProgress, Updater, VerifiedUpdate};

#[derive(Default)]
struct State {
    phase: &'static str,
    offer: Option<VerifiedUpdate>,
    staged: Option<Arc<StagedUpdate>>,
    progress: Option<UpdateProgress>,
    error: Option<&'static str>,
    restart_required: bool,
    observation_sequence: u64,
}

impl State {
    fn restore(&mut self) {
        self.phase = if self.staged.is_some() {
            "ready"
        } else if self.offer.is_some() {
            "available"
        } else {
            "idle"
        };
        self.progress = None;
    }
}

struct Action {
    _guard: tokio::sync::OwnedMutexGuard<()>,
    state: Arc<Mutex<State>>,
}
impl Drop for Action {
    fn drop(&mut self) {
        let mut state = self.state.lock();
        if matches!(state.phase, "checking" | "downloading" | "installing") {
            state.restore();
            state.error = Some("The update could not finish. Please try again later.");
        }
    }
}
struct CancelOnDrop {
    token: CancellationToken,
    armed: bool,
}
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.token.cancel();
        }
    }
}

/// Cloneable application service; status is memory-only and bounded. The
/// constructor touches the private update journal, so initialize it off UI.
#[derive(Clone)]
pub struct Service {
    updater: Option<Updater>,
    state: Arc<Mutex<State>>,
    action: Arc<tokio::sync::Mutex<()>>,
}

impl Service {
    pub fn new(data: PathBuf, executable: PathBuf) -> Result<Self> {
        Ok(Self::from_updater(Updater::from_embedded(
            data.join("updates"),
            executable,
        )?))
    }

    fn from_updater(updater: Option<Updater>) -> Self {
        let phase = if updater.is_some() {
            "idle"
        } else {
            "disabled"
        };
        Self {
            updater,
            state: Arc::new(Mutex::new(State {
                phase,
                ..State::default()
            })),
            action: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// No filesystem, network, or unbounded progress queue is involved.
    pub fn status(&self) -> Value {
        let mut state = self.state.lock();
        state.observation_sequence = state.observation_sequence.saturating_add(1);
        let active = matches!(state.phase, "checking" | "downloading" | "installing");
        let enabled = self.updater.is_some();
        json!({
            "sequence": state.observation_sequence,
            "enabled": enabled,
            "phase": state.phase,
            "available": state.offer.as_ref().map(VerifiedUpdate::summary),
            "progress": state.progress,
            "message": state.error.or(if !enabled {Some("Updates are not available in this build.")} else {None}),
            "restart_required": state.restart_required,
            "can_check": enabled && !active && !state.restart_required,
            "can_download": enabled && !active && state.offer.is_some() && state.staged.is_none() && !state.restart_required,
            "can_install": enabled && !active && state.staged.is_some() && !state.restart_required,
        })
    }

    fn admit(&self) -> Result<Action> {
        let guard = self
            .action
            .clone()
            .try_lock_owned()
            .map_err(|_| anyhow::anyhow!("Another update action is already in progress."))?;
        ensure!(
            !self.state.lock().restart_required,
            "Restart Xylo to finish installing the update."
        );
        Ok(Action {
            _guard: guard,
            state: self.state.clone(),
        })
    }

    fn failed(&self, error: anyhow::Error) -> anyhow::Error {
        let message = workbench_distribution::product_error(&error);
        tracing::debug!(error = ?error, "Application update action failed");
        let mut state = self.state.lock();
        state.restore();
        state.error = Some(message);
        error.context(message)
    }

    async fn execute(
        &self,
        cancel: CancellationToken,
        phase: &'static str,
        operation: impl FnOnce(&Updater, &CancellationToken, &Service) -> Result<()> + Send + 'static,
    ) -> Result<Value> {
        let guard = self.admit()?;
        let Some(updater) = self.updater.clone() else {
            return Ok(self.status());
        };
        {
            let mut state = self.state.lock();
            state.phase = phase;
            state.error = None;
            state.progress = None;
        }
        let mut cancellation = CancelOnDrop {
            token: cancel.clone(),
            armed: true,
        };
        let service = self.clone();
        // The action guard and final state transitions belong to the blocking
        // job. Dropping/aborting an awaiting host task cannot admit a concurrent
        // action while the old job is still reading or replacing files.
        let result = tokio::task::spawn_blocking(move || {
            let _guard = guard;
            operation(&updater, &cancel, &service).map_err(|error| service.failed(error))?;
            Ok(service.status())
        })
        .await;
        cancellation.armed = false;
        result.map_err(|error| {
            self.failed(anyhow::Error::new(error).context("The update could not finish."))
        })?
    }

    pub async fn check(&self, cancel: CancellationToken) -> Result<Value> {
        self.execute(cancel, "checking", |updater, cancel, service| {
            let offer = updater.check(cancel)?;
            let mut state = service.state.lock();
            state.offer = offer;
            state.staged = None;
            state.phase = if state.offer.is_some() {
                "available"
            } else {
                "up_to_date"
            };
            Ok(())
        })
        .await
    }

    pub async fn stage(&self, cancel: CancellationToken) -> Result<Value> {
        self.execute(cancel, "downloading", |updater, cancel, service| {
            let offer = service
                .state
                .lock()
                .offer
                .clone()
                .context("Check for an available update first.")?;
            let already_staged = service.state.lock().staged.is_some();
            if already_staged {
                service.state.lock().phase = "ready";
                return Ok(());
            }
            let staged = updater.stage(&offer, cancel, |progress| {
                service.state.lock().progress = Some(progress);
            })?;
            let mut state = service.state.lock();
            state.staged = Some(Arc::new(staged));
            state.phase = "ready";
            Ok(())
        })
        .await
    }

    /// Host MUST save drafts and quiesce tasks/helpers before this explicit
    /// action. Do not quit GPUI first: failed installs must recover controls.
    pub async fn install(&self, cancel: CancellationToken) -> Result<Value> {
        self.execute(cancel, "installing", |updater, cancel, service| {
            let staged = service
                .state
                .lock()
                .staged
                .clone()
                .context("Download and verify the update first.")?;
            let outcome = updater.install_staged(&staged, cancel)?;
            let mut state = service.state.lock();
            state.phase = "installed";
            state.restart_required = outcome.restart_required;
            state.staged = None;
            state.offer = None;
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine as _, engine::general_purpose::STANDARD};

    fn signed_candidate() -> (tempfile::TempDir, Service) {
        let directory = tempfile::tempdir().unwrap();
        let key = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        let sign = |bytes: &[u8]| {
            STANDARD.encode(
                minisign::sign(
                    Some(&key.pk),
                    &key.sk,
                    std::io::Cursor::new(bytes),
                    Some("test fixture only"),
                    None,
                )
                .unwrap()
                .to_string(),
            )
        };
        let policy = serde_json::from_value(json!({
            "enabled": true, "application_id": "com.qurkos.xylo", "channel": "stable",
            "manifest_url": "https://updates.example.test/stable.json",
            "signature_url": "https://updates.example.test/stable.json.sig",
            "public_key": STANDARD.encode(key.pk.to_box().unwrap().to_bytes()),
            "allowed_origins": ["https://updates.example.test"], "max_artifact_bytes": 1048576,
            "macos_team_id": "TEST000000"
        }))
        .unwrap();
        let target = workbench_distribution::native_target().unwrap();
        let format = if target.starts_with("macos-") {
            "app"
        } else if target.starts_with("linux-") {
            "appimage"
        } else {
            "nsis"
        };
        let updater = Updater::new(
            policy,
            "0.1.0".parse().unwrap(),
            target.clone(),
            directory.path().join("updates"),
            std::env::current_exe().unwrap(),
        )
        .unwrap();
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        use sha2::Digest;
        let digest = sha2::Sha256::digest(b"package")
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let metadata = serde_json::to_vec(&json!({
            "schema": 1, "application_id": "com.qurkos.xylo", "channel": "stable", "sequence": 2,
            "version": "0.2.0", "published_at": at-30, "expires_at": at+3600, "notes": "Test update",
            "platforms": {target: {"url": "https://updates.example.test/private-artifact", "signature": sign(b"package"), "format": format, "size_bytes": 7, "sha256": digest}}
        })).unwrap();
        let offer = updater
            .verify_manifest(&metadata, &sign(&metadata), at)
            .unwrap()
            .unwrap();
        let service = Service::from_updater(Some(updater));
        service.state.lock().offer = Some(offer);
        service.state.lock().phase = "available";
        (directory, service)
    }

    #[tokio::test]
    async fn disabled_build_has_no_fake_update_success_or_network() {
        let data = tempfile::tempdir().unwrap();
        let service =
            Service::new(data.path().to_path_buf(), std::env::current_exe().unwrap()).unwrap();
        // Default builds deliberately have no maintainer key/feed. Test release
        // builds with a supplied private policy through their own qualification.
        let service = if service.updater.is_some() {
            Service::from_updater(None)
        } else {
            service
        };
        let before = service.status();
        assert_eq!(before["phase"], "disabled");
        assert_eq!(before["can_install"], false);
        let cancel = CancellationToken::new();
        let checked = service.check(cancel.clone()).await.unwrap();
        let staged = service.stage(cancel.clone()).await.unwrap();
        let installed = service.install(cancel).await.unwrap();
        assert_eq!(checked["phase"], "disabled");
        assert_eq!(staged["phase"], "disabled");
        assert_eq!(installed["phase"], "disabled");
        assert!(before["sequence"].as_u64() < checked["sequence"].as_u64());
        assert!(checked["sequence"].as_u64() < staged["sequence"].as_u64());
        assert!(staged["sequence"].as_u64() < installed["sequence"].as_u64());
        assert!(!data.path().join("updates").exists());
    }

    #[tokio::test]
    async fn actions_are_exclusive_and_restart_remains_an_explicit_boundary() {
        let service = Service::from_updater(None);
        let _held = service.action.lock().await;
        assert!(
            service
                .check(CancellationToken::new())
                .await
                .unwrap_err()
                .to_string()
                .contains("in progress")
        );
        drop(_held);
        service.state.lock().restart_required = true;
        assert!(
            service
                .install(CancellationToken::new())
                .await
                .unwrap_err()
                .to_string()
                .contains("Restart")
        );
    }

    #[tokio::test]
    async fn cancelled_download_retains_real_authenticated_candidate_without_fake_ready() {
        let (_directory, service) = signed_candidate();
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(
            service
                .stage(cancel)
                .await
                .unwrap_err()
                .to_string()
                .contains("cancelled")
        );
        let status = service.status();
        assert_eq!(status["phase"], "available");
        assert_eq!(status["can_download"], true);
        assert_eq!(status["can_install"], false);
        assert_eq!(status["available"]["version"], "0.2.0");
        assert!(!status.to_string().contains("updates.example.test"));
        assert!(service.state.lock().offer.is_some());
    }

    #[tokio::test]
    async fn aborted_waiter_cancels_io_and_keeps_action_excluded_until_job_finishes() {
        let (_directory, service) = signed_candidate();
        let (began_tx, began_rx) = tokio::sync::oneshot::channel();
        let (cancelled_tx, cancelled_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel();
        let running = service.clone();
        let task = tokio::spawn(async move {
            running
                .execute(CancellationToken::new(), "checking", move |_, cancel, _| {
                    let _ = began_tx.send(());
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
                    while !cancel.is_cancelled() && std::time::Instant::now() < deadline {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                    ensure!(cancel.is_cancelled(), "Abandoned update was not cancelled");
                    let _ = cancelled_tx.send(());
                    finish_rx.recv_timeout(std::time::Duration::from_secs(2))?;
                    anyhow::bail!("Update cancelled")
                })
                .await
        });
        began_rx.await.unwrap();
        task.abort();
        let _ = task.await;
        cancelled_rx.await.unwrap();
        assert!(
            service
                .stage(CancellationToken::new())
                .await
                .unwrap_err()
                .to_string()
                .contains("in progress")
        );
        finish_tx.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while service.status()["phase"] == "checking" {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(service.status()["phase"], "available");
        assert_eq!(service.status()["can_install"], false);
        assert!(service.state.lock().offer.is_some());
    }
}
