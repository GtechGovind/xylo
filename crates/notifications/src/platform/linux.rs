//! XDG notification D-Bus API with one async connection/listener, not a native
//! thread for every toast. The daemon's actions capability controls routing.
use super::*;
use futures_util::StreamExt;
use std::{
    collections::{BTreeMap, VecDeque},
    sync::atomic::{AtomicBool, Ordering},
};
use zbus::{Connection, Proxy, zvariant::OwnedValue};

pub(super) struct Native {
    identity: AppIdentity,
    connection: OnceCell<Connection>,
    ids: Arc<std::sync::Mutex<VecDeque<(String, u32)>>>,
    sender: async_channel::Sender<String>,
    actions: Arc<AtomicBool>,
    shutdown: CancellationToken,
}
impl Native {
    pub fn create(identity: AppIdentity) -> (Arc<dyn Adapter>, Receiver<String>) {
        let (sender, receiver) = async_channel::bounded(64);
        (
            Arc::new(Self {
                identity,
                connection: OnceCell::new(),
                ids: Arc::new(std::sync::Mutex::new(VecDeque::new())),
                sender,
                actions: Arc::new(AtomicBool::new(false)),
                shutdown: CancellationToken::new(),
            }),
            receiver,
        )
    }
    async fn connection(&self, cancel: &CancellationToken) -> Result<&Connection> {
        self.connection.get_or_try_init(||async {
            let connection=tokio::select! {
                _=cancel.cancelled()=>anyhow::bail!("Notification action cancelled"),
                value=tokio::time::timeout(Duration::from_secs(3),Connection::session())=>value??,
            };
            let daemon=proxy(&connection).await?;
            self.refresh_capabilities(&daemon).await?;
            let listener=connection.clone();
            let sender=self.sender.clone();
            let ids=self.ids.clone();
            let shutdown=self.shutdown.child_token();
            let listener_guard=shutdown.clone().drop_guard();
            let supported=self.actions.clone();
            let(ready,started)=tokio::sync::oneshot::channel();
            tokio::spawn(async move {
                let Ok(daemon)=proxy(&listener).await else{return;};
                let Ok(mut actions)=daemon.receive_signal("ActionInvoked").await else{return;};
                let Ok(mut owners)=daemon.receive_owner_changed().await else{return;};
                let _=ready.send(());
                loop {
                    tokio::select! {
                        biased;
                        _=shutdown.cancelled()=>break,
                        owner=owners.next()=>{
                            if owner.is_none(){break;}
                            // IDs are unique only within a daemon's lifetime.
                            // Never route another app's reused ID after restart.
                            if let Ok(mut ids)=ids.lock(){ids.clear();}
                            supported.store(false,Ordering::Release);
                        }
                        signal=actions.next()=>{
                            let Some(signal)=signal else{break;};
                            let Ok((id,action))=signal.body().deserialize::<(u32,String)>() else{continue;};
                            if action!="default"{continue;}
                            let tag=ids.lock().ok().and_then(|ids|ids.iter().find(|(_,known)|*known==id).map(|(tag,_)|tag.clone()));
                            if let Some(tag)=tag{let _=sender.try_send(tag);}
                        }
                    }
                }
            });
            tokio::time::timeout(Duration::from_secs(3),started).await??;
            listener_guard.disarm();
            drop(daemon);
            Ok::<_,anyhow::Error>(connection)
        }).await
    }
    async fn refresh_capabilities(&self, daemon: &Proxy<'_>) -> Result<()> {
        let capabilities: Vec<String> =
            tokio::time::timeout(Duration::from_secs(3), daemon.call("GetCapabilities", &()))
                .await??;
        self.actions.store(
            capabilities.iter().any(|value| value == "actions"),
            Ordering::Release,
        );
        Ok(())
    }
}
impl Drop for Native {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}
async fn proxy(connection: &Connection) -> Result<Proxy<'_>> {
    Ok(Proxy::new(
        connection,
        "org.freedesktop.Notifications",
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
    )
    .await?)
}
#[async_trait]
impl Adapter for Native {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            request_permission: false,
            live_activation: self.actions.load(Ordering::Acquire),
            cold_activation: false,
            retraction: true,
        }
    }
    async fn permission(&self, cancel: CancellationToken) -> Result<Permission> {
        match self.connection(&cancel).await {
            Ok(connection) => {
                let result =
                    async { self.refresh_capabilities(&proxy(connection).await?).await }.await;
                if result.is_ok() {
                    Ok(Permission::NotRequired)
                } else {
                    Ok(Permission::Unavailable)
                }
            }
            Err(error) if cancel.is_cancelled() => Err(error),
            Err(error) => {
                tracing::debug!(?error, "Native notification daemon unavailable");
                Ok(Permission::Unavailable)
            }
        }
    }
    async fn request_permission(&self, cancel: CancellationToken) -> Result<Permission> {
        self.permission(cancel).await
    }
    async fn post(&self, notice: NativeNotice, cancel: CancellationToken) -> Result<()> {
        let connection = self.connection(&cancel).await?;
        let proxy = proxy(connection).await?;
        let previous = self
            .ids
            .lock()
            .map_err(|_| anyhow::anyhow!("Notification state unavailable"))?
            .iter()
            .find(|(tag, _)| tag == &notice.tag)
            .map(|(_, id)| *id)
            .unwrap_or(0);
        let actions: Vec<&str> = if self.actions.load(Ordering::Acquire) {
            vec!["default", "Open Xylo"]
        } else {
            vec![]
        };
        let hints: BTreeMap<&str, OwnedValue> =
            BTreeMap::from([("suppress-sound", OwnedValue::from(true))]);
        // Once Notify has been sent, collect its bounded reply even when the
        // caller cancels. The returned ID is needed to retract an accepted
        // notification; dropping the request would lose that cleanup handle.
        ensure!(!cancel.is_cancelled(), "Notification delivery cancelled");
        let id: u32 = tokio::time::timeout(
            Duration::from_secs(5),
            proxy.call(
                "Notify",
                &(
                    self.identity.name.as_str(),
                    previous,
                    "",
                    notice.title,
                    notice.body,
                    actions,
                    hints,
                    5000i32,
                ),
            ),
        )
        .await??;
        if cancel.is_cancelled() {
            let _ = tokio::time::timeout(
                Duration::from_secs(3),
                proxy.call::<_, _, ()>("CloseNotification", &(id,)),
            )
            .await;
            anyhow::bail!("Notification delivery cancelled");
        }
        let retired = {
            let mut ids = self
                .ids
                .lock()
                .map_err(|_| anyhow::anyhow!("Notification state unavailable"))?;
            remember(&mut ids, notice.tag, id)
        };
        if let Some(retired) = retired {
            let _ = tokio::time::timeout(
                Duration::from_secs(3),
                proxy.call::<_, _, ()>("CloseNotification", &(retired,)),
            )
            .await;
        }
        Ok(())
    }
    async fn dismiss(&self, tag: String, cancel: CancellationToken) -> Result<()> {
        let id = {
            let mut ids = self
                .ids
                .lock()
                .map_err(|_| anyhow::anyhow!("Notification state unavailable"))?;
            ids.iter()
                .position(|(known, _)| known == &tag)
                .and_then(|position| ids.remove(position))
                .map(|(_, id)| id)
        };
        if let Some(id) = id {
            let proxy = proxy(self.connection(&cancel).await?).await?;
            let _: () = tokio::time::timeout(
                Duration::from_secs(3),
                proxy.call("CloseNotification", &(id,)),
            )
            .await??;
        }
        Ok(())
    }
}

fn remember(ids: &mut VecDeque<(String, u32)>, tag: String, id: u32) -> Option<u32> {
    ids.retain(|(known, number)| known != &tag && *number != id);
    ids.push_back((tag, id));
    if ids.len() > 128 {
        ids.pop_front().map(|(_, id)| id)
    } else {
        None
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reused_daemon_id_replaces_old_route_and_active_routes_stay_bounded() {
        let mut ids = VecDeque::new();
        assert_eq!(remember(&mut ids, "old".into(), 12), None);
        assert_eq!(remember(&mut ids, "new".into(), 12), None);
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0].0, "new");
        for id in 13..180 {
            remember(&mut ids, id.to_string(), id);
        }
        assert_eq!(ids.len(), 128);
    }
}
