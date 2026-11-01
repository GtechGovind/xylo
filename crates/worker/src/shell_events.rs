//! Coalesces shell snapshots without losing lifecycle state when output is busy.
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, atomic::AtomicU64},
};
use tokio::sync::mpsc;
use xylo_core::{EngineEvent, Envelope};
use xylo_engine::tools::shell_jobs::JobEvents;

pub(super) fn sink(
    output: mpsc::Sender<Envelope<EngineEvent>>,
    sequence: Arc<AtomicU64>,
) -> JobEvents {
    let pending = Arc::new(Mutex::new(HashMap::new()));
    let (changed, mut changes) = mpsc::channel(1);
    let waiting = pending.clone();
    tokio::spawn(async move {
        while changes.recv().await.is_some() {
            let updates = std::mem::take(&mut *waiting.lock().unwrap());
            for ((session, _), event) in updates {
                if super::send(&output, &sequence, super::uuid_nil(), Some(session), event)
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
    });
    Arc::new(move |session, event| {
        if let EngineEvent::ShellJobUpdated { job } = &event {
            pending.lock().unwrap().insert((session, job.id), event);
            // A single queued wakeup covers all currently pending snapshots.
            // The receiver sends them in order, so completion cannot be
            // followed by an older live snapshot of the same job.
            let _ = changed.try_send(());
        }
    })
}

#[cfg(test)]
mod tests;
