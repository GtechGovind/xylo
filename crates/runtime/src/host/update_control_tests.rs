//! Real host lock/cancellation regression: no production signing key required.
use super::*;

#[tokio::test]
async fn shutdown_cancels_a_read_holder_before_waiting_for_transient_install_writer() {
    let (_directory, host, _events) = crate::host::tests::state();
    let session = Uuid::new_v4();
    let queued = CancellationToken::new();
    let capability = CancellationToken::new();
    let install = CancellationToken::new();
    let install_id = Uuid::new_v4();
    host.queued
        .lock()
        .insert(session, (Uuid::new_v4(), queued.clone()));
    {
        let mut requests = host.capability_requests.lock().await;
        requests.insert(Uuid::new_v4(), capability.clone());
        requests.insert(install_id, install.clone());
    }
    // Model a Run blocked in admission while holding handle()'s read guard.
    // Keep the guard after cancellation so the test can inspect shutdown's
    // cancellation order before allowing its writer to proceed.
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let reader_host = host.clone();
    let reader_cancel = queued.clone();
    let reader = tokio::spawn(async move {
        let _operation = reader_host.operations.read().await;
        let _ = entered_tx.send(());
        reader_cancel.cancelled().await;
        let _ = release_rx.await;
    });
    entered_rx.await.unwrap();
    host.installing.store(true, Ordering::Release);
    *host.install_request.lock() = Some(install_id);
    let closing = host.clone();
    let shutdown = tokio::spawn(async move { closing.shutdown().await });
    tokio::time::timeout(Duration::from_secs(2), async {
        queued.cancelled().await;
        capability.cancelled().await;
    })
    .await
    .expect("Shutdown waited for the writer before cancelling its read holder");
    assert!(
        !install.is_cancelled(),
        "The active install must not be cancelled while its writer is still awaited"
    );
    assert!(!shutdown.is_finished());
    // The real installer may fail its initial idle check and clear this flag
    // while shutdown is already waiting. That must not strand the writer.
    host.installing.store(false, Ordering::Release);
    host.install_request.lock().take();
    release_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), shutdown)
        .await
        .unwrap()
        .unwrap();
    reader.await.unwrap();
    assert!(host.stopping.load(Ordering::Acquire));
    assert!(install.is_cancelled()); // Only after the install gate was released.
}
