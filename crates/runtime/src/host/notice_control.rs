//! Host-only notification routing. Only live worker envelopes with a matching
//! transport request produce lifecycle notices; history replay never does.
use super::*;
use crate::notifications::{Kind, NoticeEvent, UserAction};

pub(super) fn emit(
    sender: &Sender<NoticeEvent>,
    foreground: &AtomicBool,
    session_id: Uuid,
    turn_id: Uuid,
    event: &EngineEvent,
    scheduled: bool,
) {
    let (kind, event_id) = match event {
        EngineEvent::Started => (Kind::Started, Uuid::new_v5(&turn_id, b"notice-started")),
        EngineEvent::ApprovalRequired { action_id, .. } => (Kind::Approval, *action_id),
        EngineEvent::Completed { .. } => (
            if scheduled {
                Kind::ScheduleCompleted
            } else {
                Kind::Completed
            },
            Uuid::new_v5(&turn_id, b"notice-terminal"),
        ),
        EngineEvent::Error { .. } => (
            if scheduled {
                Kind::ScheduleFailed
            } else {
                Kind::Failed
            },
            Uuid::new_v5(&turn_id, b"notice-terminal"),
        ),
        EngineEvent::Cancelled => (Kind::Cancelled, Uuid::new_v5(&turn_id, b"notice-terminal")),
        _ => return,
    };
    // Delivery is best effort. A stalled desktop notification daemon must never
    // block task streaming, cancellation, durable receipts or process reaping.
    if sender
        .try_send(NoticeEvent {
            session_id,
            turn_id,
            event_id,
            kind,
            occurred_at_ms: now_ms(),
            background: !foreground.load(Ordering::Acquire),
        })
        .is_err()
    {
        tracing::debug!("Notification queue is busy; lifecycle notice skipped");
    }
}
impl HostState {
    pub(super) fn start_notifications(
        self: &Arc<Self>,
        incoming: Receiver<NoticeEvent>,
        cancel: CancellationToken,
    ) -> [tokio::task::JoinHandle<()>; 2] {
        let service = self.notifications.clone();
        let stop = cancel.clone();
        let delivery = tokio::spawn(async move {
            if let Err(error) = service.initialize().await {
                tracing::debug!(%error,"Load notification preferences");
            }
            loop {
                let event = tokio::select! {_=stop.cancelled()=>break,event=incoming.recv()=>match event{Ok(event)=>event,Err(_)=>break}};
                if let Err(error) = service.observe(event, stop.clone()).await {
                    tracing::debug!(%error,"Native notification delivery unavailable");
                }
            }
        });
        let service = self.notifications.clone();
        let events = self.events.clone();
        let store = self.store.clone();
        let activation = tokio::spawn(async move {
            loop {
                let activation = match service.next_activation(cancel.clone()).await {
                    Ok(Some(activation)) => activation,
                    Ok(None) => break,
                    Err(error) => {
                        tracing::debug!(%error,"Native notification activation unavailable");
                        break;
                    }
                };
                let lookup = store.clone();
                let session = activation.session_id;
                let conversation =
                    tokio::task::spawn_blocking(move || lookup.conversation(session))
                        .await
                        .ok()
                        .and_then(Result::ok)
                        .flatten();
                if events
                    .send(HostEvent::NotificationActivation {
                        activation,
                        conversation,
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        [delivery, activation]
    }
    pub(super) async fn notification_action(
        &self,
        name: &str,
        args: &Value,
        cancel: CancellationToken,
    ) -> Result<Value> {
        let snapshot = match name {
            "notifications_status" => self.notifications.initialize().await?,
            "notifications_refresh" => self.notifications.refresh_permission(cancel).await?,
            "notifications_preferences" => {
                self.notifications
                    .set_preferences(serde_json::from_value(
                        args.get("preferences")
                            .context("Choose notification preferences")?
                            .clone(),
                    )?)
                    .await?
            }
            "notifications_request_permission" => {
                self.notifications
                    .request_permission(UserAction::RequestPermission, cancel)
                    .await?
            }
            "notifications_test" => {
                let outcome = self
                    .notifications
                    .test_notification(UserAction::SendTest, cancel)
                    .await?;
                return Ok(
                    serde_json::json!({"snapshot":self.notifications.status(),"outcome":outcome}),
                );
            }
            _ => anyhow::bail!("This notification action is not available"),
        };
        Ok(serde_json::json!({"snapshot":snapshot}))
    }
}

#[cfg(test)]
mod tests;
