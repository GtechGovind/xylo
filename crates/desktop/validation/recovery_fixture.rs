//! Shared offline fixture: real Store/Host, controlled delivery loss, no model.
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use uuid::Uuid;
use workbench_core::{AppSettings, TurnRequest};
use workbench_runtime::{
    AppPaths,
    host::{HostClient, HostEvent},
};
use workbench_services::{ActionIntent, Store};

#[derive(Default)]
pub struct Delivery {
    pub drop_next: AtomicBool,
    pub drop_form_next: AtomicBool,
    pub form_dropped: AtomicBool,
    pub dropped: AtomicBool,
    pub pause_pending: AtomicBool,
    pub locked: AtomicBool,
    pub release: AtomicBool,
}
pub struct Profile {
    pub directory: tempfile::TempDir,
    pub paths: AppPaths,
    pub workspace: PathBuf,
    pub session: Uuid,
    pub turn: Uuid,
    pub actions: Vec<Uuid>,
    pub store: Store,
}
pub fn seeded(count: usize) -> Profile {
    let directory = tempfile::tempdir().unwrap();
    let base = directory.path().canonicalize().unwrap();
    let workspace = base.join("project");
    std::fs::create_dir(&workspace).unwrap();
    let paths = AppPaths::at(base.join("profile")).unwrap();
    let store = Store::open(&paths.database).unwrap();
    store
        .save_settings(&AppSettings {
            last_workspace: Some(workspace.clone()),
            ..Default::default()
        })
        .unwrap();
    let session = Uuid::new_v4();
    let turn = Uuid::new_v4();
    store
        .register_user_turn(&TurnRequest {
            session_id: session,
            turn_id: turn,
            prompt: "Inspect an interrupted project update".into(),
            workspace: workspace.clone(),
            provider: AppSettings::default().provider,
            permissions: AppSettings::default().permissions,
            attachments: vec![],
        })
        .unwrap();
    let mut profile = Profile {
        directory,
        paths,
        workspace,
        session,
        turn,
        actions: vec![],
        store,
    };
    for index in 0..count {
        seed_action(&mut profile, "file_write", true, index);
    }
    profile
}
pub fn seed_action(profile: &mut Profile, name: &str, qualified: bool, index: usize) -> Uuid {
    let id = Uuid::new_v4();
    let lease = profile.store.action_execution_lease(id, qualified).unwrap();
    let filename = format!("partial-{index}.txt");
    let artifact = profile
        .paths
        .artifacts
        .join(format!("retained-{index}.txt"));
    std::fs::write(profile.workspace.join(&filename), "partial content remains").unwrap();
    std::fs::write(&artifact, "retained observation file").unwrap();
    profile
        .store
        .begin_action(&ActionIntent {
            id,
            session_id: profile.session,
            turn_id: profile.turn,
            name: name.into(),
            arguments: serde_json::json!({"path":filename,"content":"intended completed content"}),
            writes: true,
            idempotency_key: id.to_string(),
        })
        .unwrap();
    profile.store.mark_action_running(id).unwrap();
    let summary = if name == "plugin_run_installed" {
        // A structured native summary with no human output must not become a
        // raw internal payload dump in the normal product observation view.
        serde_json::to_string(
            &serde_json::json!({"id":id,"fuel_remaining":42,"internal_status":"unverified"}),
        )
        .unwrap()
    } else {
        "The write stopped after partial content was saved.".into()
    };
    profile
        .store
        .mark_action_unknown_with_result(
            id,
            &serde_json::json!({"status":"outcome_unknown",
        "summary":summary,"artifacts":[artifact]}),
            "Interruption after a partial write",
        )
        .unwrap();
    drop(lease);
    profile.actions.push(id);
    id
}
pub fn host(
    profile: &Profile,
    delivery: Arc<Delivery>,
) -> (HostClient, async_channel::Receiver<HostEvent>) {
    let (client, incoming) = HostClient::spawn_at(profile.paths.clone()).unwrap();
    let (sender, receiver) = async_channel::bounded(256);
    let database = profile.paths.database.clone();
    std::thread::spawn(move || {
        while let Ok(event) = incoming.recv_blocking() {
            if matches!(event, HostEvent::ActionReviewFormSaved { .. })
                && delivery.drop_form_next.swap(false, Ordering::SeqCst)
            {
                delivery.form_dropped.store(true, Ordering::SeqCst);
                continue;
            }
            if matches!(event, HostEvent::ActionResolved { .. })
                && delivery.drop_next.swap(false, Ordering::SeqCst)
            {
                delivery.dropped.store(true, Ordering::SeqCst);
                continue;
            }
            if matches!(&event, HostEvent::ActionReviewFormSaved { result: Ok(saved), .. } if saved.snapshot.form.pending.is_some())
                && delivery.pause_pending.swap(false, Ordering::SeqCst)
            {
                let control = delivery.clone();
                let database = database.clone();
                std::thread::spawn(move || {
                    let connection = rusqlite::Connection::open(database).unwrap();
                    connection.busy_timeout(Duration::from_secs(5)).unwrap();
                    connection.execute_batch("BEGIN IMMEDIATE").unwrap();
                    control.locked.store(true, Ordering::SeqCst);
                    while !control.release.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    connection.execute_batch("ROLLBACK").unwrap();
                });
                while !delivery.locked.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
            let stopped = matches!(event, HostEvent::Stopped);
            if sender.send_blocking(event).is_err() || stopped {
                break;
            }
        }
    });
    (client, receiver)
}
