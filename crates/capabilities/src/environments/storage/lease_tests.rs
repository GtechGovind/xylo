use super::*;

#[tokio::test]
async fn a_live_environment_lease_remains_held_while_a_waiting_caller_cancels() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("operation.lock");
    let owner = open_lock(&path).unwrap();
    assert!(try_lease(&owner).unwrap());
    assert!(!try_lease(&open_lock(&path).unwrap()).unwrap());
    let cancel = CancellationToken::new();
    let waiting = lease(path.clone(), &cancel);
    tokio::pin!(waiting);
    tokio::select! {
        result = &mut waiting => panic!("A live lease must keep the caller queued: {result:?}"),
        _ = tokio::time::sleep(Duration::from_millis(30)) => {},
    }
    cancel.cancel();
    let error = tokio::time::timeout(Duration::from_secs(2), waiting)
        .await
        .expect("A queued lease must observe cancellation")
        .unwrap_err();
    assert!(error.to_string().contains("cancelled before admission"));
    assert!(!try_lease(&open_lock(&path).unwrap()).unwrap());
    drop(owner);
    drop(lease(path, &CancellationToken::new()).await.unwrap());
}
