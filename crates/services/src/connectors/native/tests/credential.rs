//! A blocking keychain store holding the vault key exercises the real credential
//! boundary (a keychain prompt) without opening platform UI. Cancellation must
//! return while admission follows the actual job.
use super::super::*;
use keyring_core::{CredentialStore, api::CredentialStoreApi};
use std::{
    any::Any,
    collections::HashMap,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

struct Gate {
    open: Mutex<bool>,
    changed: Condvar,
    entered: AtomicUsize,
    started: tokio::sync::Notify,
}
impl Gate {
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }
}
struct ReleaseOnDrop(Arc<Gate>);
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.release();
    }
}
struct BlockingStore(Arc<Gate>);
impl CredentialStoreApi for BlockingStore {
    fn vendor(&self) -> String {
        "local-test".into()
    }
    fn id(&self) -> String {
        "blocked-credential".into()
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn build(
        &self,
        _: &str,
        _: &str,
        _: Option<&HashMap<&str, &str>>,
    ) -> keyring_core::Result<keyring_core::Entry> {
        self.0.entered.fetch_add(1, Ordering::SeqCst);
        self.0.started.notify_one();
        let guard = self.0.open.lock().unwrap();
        let (guard, timeout) = self
            .0
            .changed
            .wait_timeout_while(guard, Duration::from_secs(15), |open| !*open)
            .unwrap();
        assert!(
            *guard && !timeout.timed_out(),
            "Fixture release was not observed"
        );
        Err(keyring_core::Error::NoEntry)
    }
}

#[tokio::test]
async fn cancelling_a_blocked_credential_read_returns_but_keeps_bounded_admission() {
    let gate = Arc::new(Gate {
        open: Mutex::new(false),
        changed: Condvar::new(),
        entered: AtomicUsize::new(0),
        started: tokio::sync::Notify::new(),
    });
    let _cleanup = ReleaseOnDrop(gate.clone());
    let store: Arc<CredentialStore> = Arc::new(BlockingStore(gate.clone()));
    let directory = tempfile::tempdir().unwrap();
    let vault = CredentialVault::with_keychain_store(directory.path(), store);
    let client = NativeConnectors::new(vault).unwrap();
    let config = ConnectorConfig {
        id: uuid::Uuid::new_v4(),
        name: "Fixture".into(),
        transport: "gmail".into(),
        endpoint: "http://127.0.0.1:1".into(),
        command: None,
        args: Vec::new(),
        credential_ref: Some("fixture".into()),
    };
    let cancel = CancellationToken::new();
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let (client, config, cancel) = (client.clone(), config.clone(), cancel.clone());
        tasks.spawn(async move {
            client
                .call(
                    &config,
                    "gmail_send_draft",
                    json!({"draft_id":"draft"}),
                    &cancel,
                )
                .await
        });
    }
    tokio::time::timeout(Duration::from_secs(5), gate.started.notified())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while client.capacity.available_permits() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(result) = tasks.join_next().await {
            let error = result.unwrap().unwrap_err();
            assert!(error.downcast_ref::<ConnectorNotDispatched>().is_some());
            assert!(
                error
                    .to_string()
                    .contains("cancelled during credential access")
            );
        }
    })
    .await
    .unwrap();
    assert_eq!(
        client.capacity.available_permits(),
        0,
        "Actual blocked credential reads retain all eight slots"
    );

    let queued_cancel = CancellationToken::new();
    let token = queued_cancel.clone();
    let (queued_client, queued_config) = (client.clone(), config.clone());
    let queued = tokio::spawn(async move {
        queued_client
            .call(
                &queued_config,
                "gmail_send_draft",
                json!({"draft_id":"queued"}),
                &token,
            )
            .await
    });
    tokio::task::yield_now().await;
    queued_cancel.cancel();
    let queued_error = tokio::time::timeout(Duration::from_secs(2), queued)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(
        queued_error
            .downcast_ref::<ConnectorNotDispatched>()
            .is_some()
    );
    assert_eq!(
        gate.entered.load(Ordering::SeqCst),
        1,
        "Key loading is serialized, so a pending prompt is never repeated"
    );

    gate.release();
    tokio::time::timeout(Duration::from_secs(5), async {
        while client.capacity.available_permits() != 8 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        gate.entered.load(Ordering::SeqCst),
        8,
        "The detached jobs finish before their slots are released"
    );
}
