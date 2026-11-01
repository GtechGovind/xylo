//! Shared offline fixture: real Store/Host with interrupted writes, no model.
use std::path::PathBuf;
use uuid::Uuid;
use xylo_core::{AppSettings, TurnRequest};
use xylo_runtime::{
    AppPaths,
    host::{HostClient, HostEvent},
};
use xylo_services::{ActionIntent, Store};

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
pub fn host(profile: &Profile) -> (HostClient, async_channel::Receiver<HostEvent>) {
    HostClient::spawn_at(profile.paths.clone()).unwrap()
}
