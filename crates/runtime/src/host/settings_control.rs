//! Reserve before spawning: scheduler order cannot reorder full-settings commits.
//! Dropping a queued ticket forwards completion only after its predecessor, so
//! cancellation cannot let later snapshots overtake an in-progress commit.
use parking_lot::Mutex;
use tokio::sync::oneshot;

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
