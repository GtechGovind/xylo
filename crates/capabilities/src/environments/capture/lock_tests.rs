use super::*;

#[tokio::test]
async fn a_live_capture_lease_keeps_a_caller_queued_until_cancellation() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = Directory::open(&temporary.path().canonicalize().unwrap()).unwrap();
    let owner = lock(&directory, &CancellationToken::new()).unwrap();
    let cancel = CancellationToken::new();
    let _cancel_on_drop = cancel.clone().drop_guard();
    let waiting_directory = directory.clone();
    let waiting_cancel = cancel.clone();
    let (entered, started) = tokio::sync::oneshot::channel();
    let mut waiting = tokio::task::spawn_blocking(move || {
        let _ = entered.send(());
        lock(&waiting_directory, &waiting_cancel)
    });
    tokio::time::timeout(Duration::from_secs(2), started)
        .await
        .unwrap()
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(30), &mut waiting)
            .await
            .is_err(),
        "The competing caller must wait on the actual held output-storage lock"
    );
    cancel.cancel();
    let error = tokio::time::timeout(Duration::from_secs(2), waiting)
        .await
        .expect("A queued capture lease must observe cancellation")
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("cancelled before dispatch"));
    let probe = directory.lock_file("storage.lock").unwrap();
    let contention = probe.try_lock_exclusive().unwrap_err();
    assert_eq!(
        contention.raw_os_error(),
        fs2::lock_contended_error().raw_os_error(),
        "Cancelling the waiter must retain the original owner's lease"
    );
    drop(probe);
    drop(owner);
    drop(lock(&directory, &CancellationToken::new()).unwrap());
}
