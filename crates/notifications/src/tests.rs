//! These tests never construct a platform adapter or call an OS notification API.
use super::*;
use std::sync::{Mutex as StdMutex, atomic::AtomicUsize};

struct Fake {
    permission: StdMutex<Permission>,
    queries: AtomicUsize,
    requests: AtomicUsize,
    posts: StdMutex<Vec<NativeNotice>>,
    dismissed: StdMutex<Vec<String>>,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    block_post: AtomicBool,
}
impl Fake {
    fn new(permission: Permission) -> Self {
        Self {
            permission: StdMutex::new(permission),
            queries: AtomicUsize::new(0),
            requests: AtomicUsize::new(0),
            posts: StdMutex::new(vec![]),
            dismissed: StdMutex::new(vec![]),
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            block_post: AtomicBool::new(false),
        }
    }
}
#[async_trait]
impl Adapter for Fake {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            request_permission: true,
            live_activation: true,
            cold_activation: false,
            retraction: true,
        }
    }
    async fn permission(&self, _: CancellationToken) -> Result<Permission> {
        self.queries.fetch_add(1, Ordering::SeqCst);
        Ok(*self.permission.lock().unwrap())
    }
    async fn request_permission(&self, _: CancellationToken) -> Result<Permission> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        Ok(*self.permission.lock().unwrap())
    }
    async fn post(&self, notice: NativeNotice, _: CancellationToken) -> Result<()> {
        self.posts.lock().unwrap().push(notice);
        self.entered.notify_one();
        // Deliberately model an OS accepting a late post after cancellation.
        if self.block_post.load(Ordering::Acquire) {
            self.release.notified().await;
        }
        Ok(())
    }
    async fn dismiss(&self, tag: String, _: CancellationToken) -> Result<()> {
        self.dismissed.lock().unwrap().push(tag);
        Ok(())
    }
}
struct Fixture {
    _dir: tempfile::TempDir,
    service: Service,
    fake: Arc<Fake>,
    clicks: async_channel::Sender<String>,
}
fn fixture(permission: Permission) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let fake = Arc::new(Fake::new(permission));
    let (clicks, incoming) = async_channel::bounded(64);
    let service = Service::with_adapter(dir.path().into(), fake.clone(), incoming);
    Fixture {
        _dir: dir,
        service,
        fake,
        clicks,
    }
}
fn enabled() -> Preferences {
    Preferences {
        enabled: true,
        ..Preferences::default()
    }
}
fn event(kind: Kind) -> NoticeEvent {
    NoticeEvent {
        session_id: Uuid::new_v4(),
        turn_id: Uuid::new_v4(),
        event_id: Uuid::new_v4(),
        kind,
        occurred_at_ms: now().unwrap(),
        background: true,
    }
}
async fn ready(f: &Fixture) {
    f.service.set_preferences(enabled()).await.unwrap();
}
async fn observe(service: &Service, event: NoticeEvent) -> Outcome {
    service
        .observe(event, CancellationToken::new())
        .await
        .unwrap()
}

#[tokio::test]
async fn disabled_default_is_lazy_and_cannot_prompt_or_post() {
    let f = fixture(Permission::NotDetermined);
    assert!(!f.service.status().preferences.enabled);
    assert_eq!(
        observe(&f.service, event(Kind::Completed)).await,
        Outcome::Suppressed
    );
    assert!(!f._dir.path().join("notifications.sqlite3").exists());
    assert_eq!(f.fake.queries.load(Ordering::SeqCst), 0);
    assert_eq!(f.fake.requests.load(Ordering::SeqCst), 0);
    assert!(f.fake.posts.lock().unwrap().is_empty());
    assert!(
        f.service
            .test_notification(UserAction::SendTest, CancellationToken::new())
            .await
            .is_err()
    );
    assert!(
        f.service
            .request_permission(UserAction::RequestPermission, CancellationToken::new())
            .await
            .is_err()
    );
}
#[tokio::test]
async fn enabling_does_not_ask_permission_or_post_and_observation_cannot_grant() {
    let f = fixture(Permission::NotDetermined);
    ready(&f).await;
    assert_eq!(f.fake.queries.load(Ordering::SeqCst), 0);
    assert_eq!(
        observe(&f.service, event(Kind::Approval)).await,
        Outcome::Suppressed
    );
    assert_eq!(f.service.status().permission, Permission::NotDetermined);
    assert_eq!(f.fake.requests.load(Ordering::SeqCst), 0);
    assert!(f.fake.posts.lock().unwrap().is_empty());
    f.service
        .request_permission(UserAction::RequestPermission, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(f.fake.requests.load(Ordering::SeqCst), 1);
    assert!(
        f.service
            .request_permission(UserAction::SendTest, CancellationToken::new())
            .await
            .is_err()
    );
}
#[tokio::test]
async fn duplicate_terminal_events_are_durable_and_first_terminal_wins() {
    let f = fixture(Permission::Allowed);
    ready(&f).await;
    let completion = event(Kind::Completed);
    assert_eq!(
        observe(&f.service, completion.clone()).await,
        Outcome::Submitted
    );
    assert_eq!(
        observe(&f.service, completion.clone()).await,
        Outcome::Duplicate
    );
    let mut failure = completion.clone();
    failure.kind = Kind::Failed;
    failure.event_id = Uuid::new_v4();
    assert_eq!(observe(&f.service, failure).await, Outcome::Duplicate);
    let (_, incoming) = async_channel::bounded(1);
    let restarted = Service::with_adapter(f._dir.path().into(), f.fake.clone(), incoming);
    assert_eq!(
        observe(&restarted, completion.clone()).await,
        Outcome::Duplicate
    );
    let mut replayed_start = completion.clone();
    replayed_start.kind = Kind::Started;
    assert_eq!(
        observe(&restarted, replayed_start).await,
        Outcome::Duplicate
    );
    let mut next_run = completion;
    next_run.turn_id = Uuid::new_v4();
    assert_eq!(observe(&restarted, next_run).await, Outcome::Submitted);
    assert_eq!(f.fake.posts.lock().unwrap().len(), 2);
}
#[tokio::test]
async fn cancellation_retracts_approval_and_blocks_late_completion_and_activation() {
    let f = fixture(Permission::Allowed);
    ready(&f).await;
    let approval = event(Kind::Approval);
    assert_eq!(
        observe(&f.service, approval.clone()).await,
        Outcome::Submitted
    );
    let tag = f.fake.posts.lock().unwrap()[0].tag.clone();
    let mut cancelled = approval.clone();
    cancelled.kind = Kind::Cancelled;
    assert_eq!(observe(&f.service, cancelled).await, Outcome::Suppressed);
    assert!(f.fake.dismissed.lock().unwrap().contains(&tag));
    let mut late = approval;
    late.kind = Kind::Completed;
    assert_eq!(observe(&f.service, late).await, Outcome::Duplicate);
    f.clicks.send(tag).await.unwrap();
    let cancel = CancellationToken::new();
    let worker = f.service.clone();
    let token = cancel.clone();
    let waiter = tokio::spawn(async move { worker.next_activation(token).await.unwrap() });
    tokio::task::yield_now().await;
    cancel.cancel();
    assert_eq!(waiter.await.unwrap(), None);
}
#[tokio::test]
async fn categories_and_foreground_policy_are_independent() {
    let f = fixture(Permission::Allowed);
    let mut preferences = enabled();
    preferences.completion = false;
    f.service.set_preferences(preferences).await.unwrap();
    assert_eq!(
        observe(&f.service, event(Kind::Completed)).await,
        Outcome::Suppressed
    );
    let mut foreground = event(Kind::Failed);
    foreground.background = false;
    assert_eq!(observe(&f.service, foreground).await, Outcome::Suppressed);
    assert_eq!(
        observe(&f.service, event(Kind::ScheduleCompleted)).await,
        Outcome::Submitted
    );
    assert_eq!(
        observe(&f.service, event(Kind::Approval)).await,
        Outcome::Submitted
    );
    assert_eq!(
        observe(&f.service, event(Kind::Failed)).await,
        Outcome::Submitted
    );
    let posts = f.fake.posts.lock().unwrap();
    assert_eq!(posts.len(), 3);
    for notice in posts.iter() {
        assert!(notice.title.len() <= 32 && notice.body.starts_with("Open Xylo"));
        assert_eq!(notice.tag.len(), 35);
        assert!(
            notice
                .tag
                .bytes()
                .all(|v| v.is_ascii_alphanumeric() || v == b'-')
        );
    }
}
#[tokio::test]
async fn denial_unavailable_and_unsupported_never_imply_delivery() {
    for permission in [
        Permission::Denied,
        Permission::Unavailable,
        Permission::Unsupported,
    ] {
        let f = fixture(permission);
        ready(&f).await;
        assert_eq!(
            observe(&f.service, event(Kind::Completed)).await,
            Outcome::Suppressed
        );
        assert!(f.fake.posts.lock().unwrap().is_empty());
        assert_eq!(f.fake.requests.load(Ordering::SeqCst), 0);
    }
}
#[tokio::test]
async fn restarted_history_and_future_clock_skew_are_not_reannounced() {
    let f = fixture(Permission::Allowed);
    ready(&f).await;
    let mut old = event(Kind::Completed);
    old.occurred_at_ms -= 121_000;
    assert_eq!(observe(&f.service, old).await, Outcome::Stale);
    let mut future = event(Kind::Completed);
    future.occurred_at_ms += 10_000;
    assert_eq!(observe(&f.service, future).await, Outcome::Stale);
    assert!(f.fake.posts.lock().unwrap().is_empty());
}
#[tokio::test]
async fn native_activation_is_validated_and_consumed_once_after_restart() {
    let f = fixture(Permission::Allowed);
    ready(&f).await;
    let completion = event(Kind::Completed);
    observe(&f.service, completion.clone()).await;
    let tag = f.fake.posts.lock().unwrap()[0].tag.clone();
    let (sender, receiver) = async_channel::bounded(4);
    let restarted = Service::with_adapter(f._dir.path().into(), f.fake.clone(), receiver);
    sender.send("untrusted-tag".into()).await.unwrap();
    sender.send(tag.clone()).await.unwrap();
    assert_eq!(
        restarted
            .next_activation(CancellationToken::new())
            .await
            .unwrap(),
        Some(Activation {
            session_id: completion.session_id,
            turn_id: completion.turn_id
        })
    );
    sender.send(tag).await.unwrap();
    sender.close();
    assert_eq!(
        restarted
            .next_activation(CancellationToken::new())
            .await
            .unwrap(),
        None
    );
}
#[tokio::test]
async fn simultaneous_repeated_events_make_only_one_native_attempt() {
    let f = fixture(Permission::Allowed);
    ready(&f).await;
    let event = event(Kind::Approval);
    let mut workers = vec![];
    for _ in 0..16 {
        let service = f.service.clone();
        let event = event.clone();
        workers.push(tokio::spawn(async move { observe(&service, event).await }));
    }
    let mut submitted = 0;
    for worker in workers {
        if worker.await.unwrap() == Outcome::Submitted {
            submitted += 1;
        }
    }
    assert_eq!(submitted, 1);
    assert_eq!(f.fake.posts.lock().unwrap().len(), 1);
}
#[tokio::test]
async fn dropped_waiter_cancels_and_retracts_late_native_acceptance() {
    let f = fixture(Permission::Allowed);
    ready(&f).await;
    f.fake.block_post.store(true, Ordering::Release);
    let service = f.service.clone();
    let worker = tokio::spawn(async move { observe(&service, event(Kind::Completed)).await });
    f.fake.entered.notified().await;
    worker.abort();
    let _ = worker.await;
    f.fake.release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if !f.fake.dismissed.lock().unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        f.fake.dismissed.lock().unwrap()[0],
        f.fake.posts.lock().unwrap()[0].tag
    );
}
#[tokio::test]
async fn disabling_never_waits_for_native_post_and_enable_cannot_revive_it() {
    let f = fixture(Permission::Allowed);
    ready(&f).await;
    f.fake.block_post.store(true, Ordering::Release);
    let service = f.service.clone();
    let worker = tokio::spawn(async move { observe(&service, event(Kind::Completed)).await });
    f.fake.entered.notified().await;
    tokio::time::timeout(
        Duration::from_secs(2),
        f.service.set_preferences(Preferences::default()),
    )
    .await
    .unwrap()
    .unwrap();
    ready(&f).await;
    f.fake.release.notify_one();
    assert_eq!(worker.await.unwrap(), Outcome::Cancelled);
    assert!(!f.fake.dismissed.lock().unwrap().is_empty());
}
#[tokio::test]
async fn explicit_test_cancellation_uses_same_late_retraction_guarantee() {
    let f = fixture(Permission::Allowed);
    ready(&f).await;
    f.fake.block_post.store(true, Ordering::Release);
    let service = f.service.clone();
    let cancel = CancellationToken::new();
    let token = cancel.clone();
    let worker = tokio::spawn(async move {
        service
            .test_notification(UserAction::SendTest, token)
            .await
            .unwrap()
    });
    f.fake.entered.notified().await;
    cancel.cancel();
    f.fake.release.notify_one();
    assert_eq!(worker.await.unwrap(), Outcome::Cancelled);
    assert_eq!(f.fake.dismissed.lock().unwrap().len(), 1);
}
#[tokio::test]
async fn persistence_failure_cannot_optimistically_enable_delivery() {
    let f = fixture(Permission::Allowed);
    f.service.initialize().await.unwrap();
    std::fs::create_dir(f._dir.path().join("notifications.sqlite3")).unwrap();
    assert!(f.service.set_preferences(enabled()).await.is_err());
    assert!(!f.service.status().preferences.enabled);
    assert_eq!(
        observe(&f.service, event(Kind::Completed)).await,
        Outcome::Suppressed
    );
    assert!(f.fake.posts.lock().unwrap().is_empty());
}
#[test]
fn journal_is_bounded_and_contains_no_freeform_content() {
    let dir = tempfile::tempdir().unwrap();
    let store = store::Store::new(dir.path());
    let at = now().unwrap();
    for _ in 0..1100 {
        let event = event(Kind::Completed);
        store.claim(&event, &tag(&event), at).unwrap();
    }
    let connection = rusqlite::Connection::open(dir.path().join("notifications.sqlite3")).unwrap();
    let count: i64 = connection
        .query_row("SELECT COUNT(*) FROM notices", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1024);
    let columns: Vec<String> = connection
        .prepare("PRAGMA table_info(notices)")
        .unwrap()
        .query_map([], |row| row.get(1))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert!(
        !columns
            .iter()
            .any(|name| matches!(name.as_str(), "title" | "body" | "prompt" | "path"))
    );
}
