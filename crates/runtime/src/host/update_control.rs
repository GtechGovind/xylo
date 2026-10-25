//! Installation is a separate, explicit host action. Existing work must finish
//! first; no update retries an uncertain write or silently cancels a task.
use super::*;

struct AdmissionReset {
    flag: Arc<AtomicBool>,
    request: Arc<parking_lot::Mutex<Option<Uuid>>>,
    keep: bool,
}
impl Drop for AdmissionReset {
    fn drop(&mut self) {
        self.request.lock().take();
        if !self.keep {
            self.flag.store(false, Ordering::Release);
        }
    }
}

impl HostState {
    pub(super) async fn install_update(&self, id: Uuid) -> Result<Value> {
        ensure!(
            !self.installing.swap(true, Ordering::AcqRel),
            "An update is already being installed"
        );
        *self.install_request.lock() = Some(id);
        let mut reset = AdmissionReset {
            flag: self.installing.clone(),
            request: self.install_request.clone(),
            keep: false,
        };
        ensure!(
            self.updates.status()["can_install"] == true,
            "Download and verify an available update first"
        );
        // Refuse queued/network work before waiting for the exclusive gate. This
        // avoids parking a writer ahead of the cancellation needed by that work.
        self.ensure_update_idle(id).await?;
        let _exclusive = self.operations.write().await;
        self.ensure_update_idle(id).await?;
        let cancel = self
            .capability_requests
            .lock()
            .await
            .get(&id)
            .cloned()
            .context("Update action is no longer pending")?;
        ensure!(!cancel.is_cancelled(), "Update cancelled");

        // All command handlers and scheduled admissions are excluded while idle
        // native processes close. A failed install leaves the host available.
        let workers: Vec<_> = self
            .workers
            .lock()
            .await
            .iter()
            .map(|(session, worker)| {
                (
                    *session,
                    worker.commands.clone(),
                    worker.stop.clone(),
                    worker.exited.clone(),
                )
            })
            .collect();
        let results = futures_util::future::join_all(workers.into_iter().map(
            |(session, commands, stop, mut exited)| async move {
                let _ = tokio::time::timeout(
                    Duration::from_secs(1),
                    commands.send(Envelope::new(
                        Uuid::new_v4(),
                        Some(session),
                        0,
                        WorkerCommand::Shutdown,
                    )),
                )
                .await;
                if !*exited.borrow() {
                    let graceful =
                        tokio::time::timeout(Duration::from_secs(6), exited.changed()).await;
                    if !matches!(graceful, Ok(Ok(()))) {
                        stop.cancel();
                        let _ =
                            tokio::time::timeout(Duration::from_secs(2), exited.changed()).await;
                    }
                }
                (session, *exited.borrow())
            },
        ))
        .await;
        let mut all_stopped = true;
        {
            let mut workers = self.workers.lock().await;
            for (session, exited) in results {
                if exited {
                    workers.remove(&session);
                } else {
                    all_stopped = false;
                }
            }
        }
        ensure!(
            all_stopped,
            "A task is still closing. Try installing again shortly"
        );
        let helpers: Vec<_> = self
            .helpers
            .lock()
            .await
            .iter()
            .map(|(path, helper)| (path.clone(), helper.clone()))
            .collect();
        let results = futures_util::future::join_all(
            helpers
                .into_iter()
                .map(|(path, helper)| async move { (path, helper.shutdown().await) }),
        )
        .await;
        let mut failure = None;
        {
            let mut helpers = self.helpers.lock().await;
            for (path, result) in results {
                match result {
                    Ok(()) => {
                        helpers.remove(&path);
                    }
                    Err(error) => {
                        failure = Some(error);
                    }
                }
            }
        }
        if let Some(error) = failure {
            return Err(error).context("A tool could not close before installation");
        }
        let result = self.updates.install(cancel).await;
        if let Ok(value) = &result {
            reset.keep = value["restart_required"] == true;
        }
        result
    }
    async fn ensure_update_idle(&self, id: Uuid) -> Result<()> {
        ensure!(
            self.queued.lock().is_empty(),
            "Finish or stop your waiting tasks before installing the update"
        );
        ensure!(
            !self
                .workers
                .lock()
                .await
                .values()
                .any(|worker| worker.active.load(Ordering::Acquire)),
            "Finish or stop your running tasks before installing the update"
        );
        ensure!(
            self.capability_requests
                .lock()
                .await
                .keys()
                .all(|pending| *pending == id),
            "Wait for your current actions to finish before installing the update"
        );
        ensure!(
            self.helpers
                .lock()
                .await
                .values()
                .all(|helper| helper.pending_count() == 0),
            "Finish downloads, recording and other active tools before installing the update"
        );
        Ok(())
    }
}

#[cfg(test)]
#[path = "update_control_tests.rs"]
mod tests;
