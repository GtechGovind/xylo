use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
fn record(token: &str) -> String {
    serde_json::json!({"type":"chatgpt","access_token":token}).to_string()
}
fn run() -> RunCredentials {
    RunCredentials::new(
        Uuid::new_v4(),
        ProviderConfig::default(),
        WorkerCredential(Some(record("old"))),
        CancellationToken::new(),
    )
}
#[tokio::test]
async fn concurrent_expiry_rejections_share_one_refresh_generation() {
    let run = Arc::new(run());
    let calls = Arc::new(AtomicUsize::new(0));
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let run = run.clone();
        let calls = calls.clone();
        tasks.push(tokio::spawn(async move {
            run.resolve_with(run.request, 0, true, move |hash| async move {
                if let Some(hash) = hash {
                    assert_eq!(hash, hex::encode(Sha256::digest(b"old")));
                    calls.fetch_add(1, Ordering::SeqCst);
                }
                tokio::task::yield_now().await;
                Ok(Some(record("new")))
            })
            .await
            .unwrap()
        }));
    }
    for task in tasks {
        let response = task.await.unwrap();
        assert_eq!(response.generation, 1);
        assert_eq!(response.credential.0, Some(record("new")));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn stale_run_future_generation_and_cancel_never_touch_credentials() {
    let run = run();
    assert!(
        run.resolve_with(Uuid::new_v4(), 0, true, |_| async { panic!("stale run") })
            .await
            .is_err()
    );
    assert!(
        run.resolve_with(run.request, 1, true, |_| async {
            panic!("future generation")
        })
        .await
        .is_err()
    );
    run.cancel.cancel();
    assert!(
        run.resolve_with(run.request, 0, true, |_| async { panic!("cancelled run") })
            .await
            .is_err()
    );
}
#[tokio::test]
async fn cancellation_during_refresh_prevents_publication() {
    let run = run();
    let cancel = run.cancel.clone();
    assert!(
        run.resolve_with(run.request, 0, true, move |_| async move {
            cancel.cancel();
            Ok(Some(record("new")))
        })
        .await
        .is_err()
    );
    assert_eq!(run.state.lock().await.generation, 0);
}
#[tokio::test]
async fn stale_generation_rechecks_signout_after_another_request_renewed() {
    let run = run();
    run.resolve_with(run.request, 0, true, |_| async { Ok(Some(record("new"))) })
        .await
        .unwrap();
    assert!(
        run.resolve_with(run.request, 0, true, |_| async {
            anyhow::bail!("signed out")
        })
        .await
        .is_err()
    );
}
#[tokio::test]
async fn account_replacement_cannot_redirect_an_existing_turn() {
    let run = run();
    let replacement =
        serde_json::json!({"type":"chatgpt","access_token":"new", "subject":"different-account"})
            .to_string();
    assert!(
        run.resolve_with(run.request, 0, false, |_| async { Ok(Some(replacement)) })
            .await
            .is_err()
    );
    assert_eq!(run.state.lock().await.generation, 0);
}
#[tokio::test]
async fn ordinary_preflight_rechecks_current_vault_even_without_generation_change() {
    let run = run();
    for _ in 0..2 {
        let response = run
            .resolve_with(run.request, 0, false, |hash| async move {
                assert!(hash.is_none());
                Ok(Some(record("old")))
            })
            .await
            .unwrap();
        assert_eq!(response.generation, 0);
    }
    assert!(
        run.resolve_with(run.request, 0, false, |_| async {
            anyhow::bail!("signed out")
        })
        .await
        .is_err()
    );
}
