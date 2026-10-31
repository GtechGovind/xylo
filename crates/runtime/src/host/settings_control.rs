//! Reserve before spawning: scheduler order cannot reorder full-settings commits.
//! Dropping a queued ticket forwards completion only after its predecessor, so
//! cancellation cannot let later snapshots overtake an in-progress commit.
use parking_lot::Mutex;
use tokio::sync::oneshot;
use xylo_core::AppSettings;

tokio::task_local! {
    /// Receipt number of the host command the current task is handling.
    static RECEIPT: u64;
}

/// Runs `work` as the handler of the command received `receipt`-th.
pub(super) async fn with_receipt<F: std::future::Future>(receipt: u64, work: F) -> F::Output {
    RECEIPT.scope(receipt, work).await
}

/// Receipt of the command this task handles; `None` outside the command loop.
pub(super) fn current_receipt() -> Option<u64> {
    RECEIPT.try_with(|receipt| *receipt).ok()
}

/// Where a settings commit takes the open-project fields (`last_workspace`
/// and `ui.open_projects`) from. Project opens run outside the settings
/// commit order, so they can land between a snapshot and its commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ProjectFields {
    /// A snapshot from the UI decides them, unless a project open received
    /// after the snapshot has already been applied.
    Snapshot,
    /// The snapshot was copied from host settings to change other fields;
    /// the live project fields always win.
    Current,
}

/// Merges the live project fields into `snapshot` when the snapshot must not
/// decide them. `latest_open` is the receipt of the newest applied project open.
pub(super) fn merge_project_fields(
    snapshot: &mut AppSettings,
    current: &AppSettings,
    fields: ProjectFields,
    receipt: Option<u64>,
    latest_open: u64,
) {
    match fields {
        ProjectFields::Current => {
            snapshot.last_workspace = current.last_workspace.clone();
            snapshot.ui.open_projects = current.ui.open_projects.clone();
        }
        ProjectFields::Snapshot if receipt.is_some_and(|receipt| latest_open > receipt) => {
            snapshot.last_workspace = current.last_workspace.clone();
            for root in &current.ui.open_projects {
                if !snapshot.ui.open_projects.contains(root) {
                    snapshot.ui.open_projects.push(root.clone());
                }
            }
        }
        ProjectFields::Snapshot => {}
    }
}

#[derive(Default)]
pub(super) struct CommitOrder {
    tail: Mutex<Option<oneshot::Receiver<()>>>,
}
pub(super) struct Ticket {
    previous: Option<oneshot::Receiver<()>>,
    complete: Option<oneshot::Sender<()>>,
}
impl CommitOrder {
    pub(super) fn reserve(&self) -> Ticket {
        let (complete, next) = oneshot::channel();
        Ticket {
            previous: self.tail.lock().replace(next),
            complete: Some(complete),
        }
    }
}
impl Ticket {
    pub(super) async fn enter(mut self) -> Self {
        if let Some(previous) = self.previous.as_mut() {
            let _ = previous.await;
        }
        self.previous = None;
        self
    }
}
impl Drop for Ticket {
    fn drop(&mut self) {
        let Some(complete) = self.complete.take() else {
            return;
        };
        if let Some(previous) = self.previous.take() {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let _ = previous.await;
                    let _ = complete.send(());
                });
            } else {
                // Also preserve order if an unpolled ticket is dropped by a
                // synchronous owner; no database or provider work runs here.
                std::thread::spawn(move || {
                    let _ = previous.blocking_recv();
                    let _ = complete.send(());
                });
            }
        } else {
            let _ = complete.send(());
        }
    }
}
