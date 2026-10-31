//! Product-facing update lifecycle. Never stores trust material in UI values.
//! The host owns admission/quiescence; this service only performs explicit
//! Check, Download, and Install actions, each on the blocking pool.
use anyhow::{Context, Result, ensure};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc};
use tokio_util::sync::CancellationToken;
use xylo_distribution::{StagedUpdate, UpdateProgress, Updater, VerifiedUpdate};

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
        Ok(Self::from_updater(Updater::for_application(
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
        let message = xylo_distribution::product_error(&error);
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
mod tests;
