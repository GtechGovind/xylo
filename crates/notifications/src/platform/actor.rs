//! One idle native thread owns non-Send framework objects. Bounded commands and
//! activation channels avoid one background thread per notification.
use super::*;
use std::sync::mpsc::{SyncSender, sync_channel};

pub(super) trait Backend {
    fn permission(&mut self, cancel: &CancellationToken) -> Result<Permission>;
    fn request_permission(&mut self, cancel: &CancellationToken) -> Result<Permission>;
    fn post(&mut self, notice: NativeNotice, cancel: &CancellationToken) -> Result<()>;
    fn dismiss(&mut self, tag: &str) -> Result<()>;
}
enum Command {
    Permission(
        bool,
        CancellationToken,
        tokio::sync::oneshot::Sender<Result<Permission>>,
    ),
    Post(
        NativeNotice,
        CancellationToken,
        tokio::sync::oneshot::Sender<Result<()>>,
    ),
    Dismiss(String, tokio::sync::oneshot::Sender<Result<()>>),
}
struct Native {
    commands: SyncSender<Command>,
}
pub(super) fn start<B: Backend + 'static>(
    identity: AppIdentity,
    build: fn(AppIdentity, async_channel::Sender<String>) -> B,
) -> Result<(Arc<dyn Adapter>, Receiver<String>)> {
    let (commands, incoming) = sync_channel(16);
    let (activations, receiver) = async_channel::bounded(64);
    std::thread::Builder::new()
        .name("native-notifications".into())
        .stack_size(512 * 1024)
        .spawn(move || {
            let mut backend = build(identity, activations);
            while let Ok(command) = incoming.recv() {
                let dispatch = || dispatch(&mut backend, command);
                #[cfg(target_os = "macos")]
                objc2::rc::autoreleasepool(|_| dispatch());
                #[cfg(target_os = "windows")]
                dispatch();
            }
        })?;
    Ok((Arc::new(Native { commands }), receiver))
}
fn dispatch(backend: &mut impl Backend, command: Command) {
    match command {
        Command::Permission(request, cancel, reply) => {
            let result = if request {
                backend.request_permission(&cancel)
            } else {
                backend.permission(&cancel)
            };
            let _ = reply.send(result);
        }
        Command::Post(notice, cancel, reply) => {
            let _ = reply.send(backend.post(notice, &cancel));
        }
        Command::Dismiss(tag, reply) => {
            let _ = reply.send(backend.dismiss(&tag));
        }
    }
}
impl Native {
    async fn permission_action(
        &self,
        request: bool,
        cancel: CancellationToken,
    ) -> Result<Permission> {
        let (reply, result) = tokio::sync::oneshot::channel();
        self.commands
            .try_send(Command::Permission(request, cancel.clone(), reply))
            .context("Notification service is busy")?;
        tokio::select! {_ = cancel.cancelled()=>anyhow::bail!("Notification action cancelled"),result=result=>result.context("Notification service stopped")?}
    }
}
#[async_trait]
impl Adapter for Native {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            request_permission: cfg!(target_os = "macos"),
            live_activation: true,
            cold_activation: false,
            retraction: true,
        }
    }
    async fn permission(&self, cancel: CancellationToken) -> Result<Permission> {
        self.permission_action(false, cancel).await
    }
    async fn request_permission(&self, cancel: CancellationToken) -> Result<Permission> {
        self.permission_action(true, cancel).await
    }
    async fn post(&self, notice: NativeNotice, cancel: CancellationToken) -> Result<()> {
        let (reply, result) = tokio::sync::oneshot::channel();
        self.commands
            .try_send(Command::Post(notice, cancel.clone(), reply))
            .context("Notification service is busy")?;
        tokio::select! {_ = cancel.cancelled()=>anyhow::bail!("Notification delivery cancelled"),result=result=>result.context("Notification service stopped")?}
    }
    async fn dismiss(&self, tag: String, cancel: CancellationToken) -> Result<()> {
        let (reply, result) = tokio::sync::oneshot::channel();
        self.commands
            .try_send(Command::Dismiss(tag, reply))
            .context("Notification service is busy")?;
        tokio::select! {_ = cancel.cancelled()=>anyhow::bail!("Notification action cancelled"),result=result=>result.context("Notification service stopped")?}
    }
}
pub(super) fn wait<T>(
    receiver: std::sync::mpsc::Receiver<T>,
    cancel: &CancellationToken,
    timeout: Duration,
) -> Result<T> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        ensure!(!cancel.is_cancelled(), "Notification action cancelled");
        ensure!(
            std::time::Instant::now() < deadline,
            "Notification center timed out"
        );
        match receiver.recv_timeout(Duration::from_millis(25)) {
            Ok(value) => return Ok(value),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => anyhow::bail!("Notification center disconnected"),
        }
    }
}
