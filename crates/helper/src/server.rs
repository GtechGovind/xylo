use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{BufReader, BufWriter},
    sync::{RwLock, mpsc},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use xylo_capabilities::{
    CapabilityConfig, CapabilityService, assets::AssetManager, audio::AudioService,
};
use xylo_helper_client::protocol::{
    self, Command, PROTOCOL_VERSION, QUEUE_CAPACITY, Request, Response,
};

const CONFIG_BYTES: u64 = 1024 * 1024;

struct State {
    data: PathBuf,
    workspace: PathBuf,
    service: RwLock<CapabilityService>,
    assets: AssetManager,
    audio: AudioService,
}

impl State {
    async fn execute(&self, name: &str, args: &Value, cancel: CancellationToken) -> Result<Value> {
        ensure!(!cancel.is_cancelled(), "Native helper request cancelled");
        if name.starts_with("helper_audio_") {
            return self.audio.execute(name, args, cancel).await;
        }
        if name.starts_with("helper_plugin_") || name.starts_with("helper_environment_") {
            return self
                .service
                .read()
                .await
                .execute_control(name, args, cancel)
                .await;
        }
        match name {
            "helper_version" => Ok(
                json!({"name":"xylo-helper","version":env!("CARGO_PKG_VERSION"),"protocol_version":PROTOCOL_VERSION}),
            ),
            "helper_status" => Ok(
                json!({"workspace":self.workspace,"data_dir":self.data,"artifacts":self.data.join("artifacts"),"models":self.data.join("models"),"capabilities":self.service.read().await.status().await}),
            ),
            "helper_tools" => Ok(
                json!({"tools":self.service.read().await.tool_definitions(),"controls":["helper_version","helper_status","helper_tools","helper_get_config","helper_configure","helper_patch_config","helper_reload_config","helper_release_resources","helper_asset_catalog","helper_asset_install","helper_asset_status","helper_asset_cancel","helper_asset_activate","helper_audio_status","helper_audio_devices","helper_audio_record_start","helper_audio_record_status","helper_audio_record_stop","helper_audio_record_cancel","helper_audio_play","helper_audio_play_stop","helper_plugin_list","helper_plugin_inspect","helper_plugin_install","helper_plugin_set_enabled","helper_plugin_set_grants","helper_plugin_remove","helper_environment_list","helper_environment_save","helper_environment_status","helper_environment_connect","helper_environment_disconnect","helper_environment_create","helper_environment_start","helper_environment_stop","helper_environment_remove_container","helper_environment_remove","helper_environment_operation","helper_environment_operations","helper_environment_resolve","helper_environment_clients","helper_environment_maintenance"]}),
            ),
            "helper_asset_catalog" => self.assets.catalog(),
            "helper_asset_install" => {
                let id = args
                    .get("id")
                    .and_then(Value::as_str)
                    .context("Select an asset ID")?;
                Ok(serde_json::to_value(self.assets.install(id)?)?)
            }
            "helper_asset_status" => {
                let id = args
                    .get("job_id")
                    .map(|value| serde_json::from_value(value.clone()))
                    .transpose()?;
                Ok(json!({"jobs":self.assets.status(id)?}))
            }
            "helper_asset_cancel" => {
                let id = serde_json::from_value(
                    args.get("job_id")
                        .context("Installation job ID required")?
                        .clone(),
                )?;
                self.assets.cancel(id)?;
                Ok(json!({"job_id":id,"cancellation_requested":true}))
            }
            "helper_asset_activate" => {
                let id = args
                    .get("id")
                    .and_then(Value::as_str)
                    .context("Select an installed asset ID")?;
                let activation = self.assets.activation(id, &cancel).await?;
                let mut service = tokio::select! {service=self.service.write()=>service,_=cancel.cancelled()=>anyhow::bail!("Asset activation cancelled")};
                let data = self.data.clone();
                let _configuration =
                    tokio::task::spawn_blocking(move || acquire_configuration_lock(&data))
                        .await??;
                let _activity = service.prepare_configuration_change().await?;
                if let Some(patch) = activation.config_patch.clone() {
                    let data = self.data.clone();
                    let workspace = self.workspace.clone();
                    *service = tokio::task::spawn_blocking(move || {
                        replace_configuration(&data, &workspace, Some((patch, true)))
                    })
                    .await??;
                }
                let embedding_changed = activation.embedding_config.is_some();
                if let Some(config) = activation.embedding_config {
                    let manager = self.assets.clone();
                    tokio::task::spawn_blocking(move || {
                        manager.save_embedding_configuration(&config)
                    })
                    .await??;
                }
                let language_servers_changed = activation.language_server.is_some();
                if let Some(server) = activation.language_server {
                    let manager = self.assets.clone();
                    tokio::task::spawn_blocking(move || {
                        manager.save_language_server_configuration(&server)
                    })
                    .await??;
                }
                Ok(
                    json!({"state":"activated","asset_id":activation.asset_id,"config_patch":activation.config_patch,"embedding_changed":embedding_changed,"language_servers_changed":language_servers_changed,"restart_required":false}),
                )
            }
            "helper_get_config" => {
                let data = self.data.clone();
                let workspace = self.workspace.clone();
                let config =
                    tokio::task::spawn_blocking(move || load_configuration(&data, &workspace))
                        .await??;
                Ok(json!({"config":config}))
            }
            "helper_configure" | "helper_patch_config" | "helper_reload_config" => {
                let provided = if name != "helper_reload_config" {
                    let value = args
                        .get("config")
                        .context("Native configuration control requires config object")?;
                    ensure!(
                        value.is_object(),
                        "Capability configuration must be an object"
                    );
                    ensure!(
                        serde_json::to_vec(value)?.len() as u64 <= CONFIG_BYTES,
                        "Capability configuration exceeds 1MiB budget"
                    );
                    Some((value.clone(), name == "helper_patch_config"))
                } else {
                    None
                };
                let mut service = tokio::select! {
                    service=self.service.write()=>service,
                    _=cancel.cancelled()=>anyhow::bail!("Native configuration change cancelled before applying"),
                };
                let configuration_data = self.data.clone();
                let _configuration = tokio::task::spawn_blocking(move || {
                    acquire_configuration_lock(&configuration_data)
                })
                .await??;
                // Hold exclusive activity ownership until the replacement is
                // persisted and installed. Busy chats must leave config intact.
                let _activity = service.prepare_configuration_change().await?;
                let data = self.data.clone();
                let workspace = self.workspace.clone();
                let replacement = tokio::task::spawn_blocking(move || {
                    replace_configuration(&data, &workspace, provided)
                })
                .await??;
                *service = replacement;
                Ok(
                    json!({"state":"configured","capabilities":service.status().await,"assets_verified":false}),
                )
            }
            "helper_release_resources" => {
                self.service.write().await.shutdown().await?;
                Ok(
                    json!({"state":"released","released":["managed_browser","managed_llama_server"],"note":"Helper shutdown releases the remaining process memory."}),
                )
            }
            _ => {
                let service = self.service.read().await;
                let observation = service.execute(name, args, cancel).await?;
                Ok(serde_json::to_value(observation)?)
            }
        }
    }
}

fn load_configuration(data: &Path, workspace: &Path) -> Result<CapabilityConfig> {
    let path = data.join("capabilities.json");
    let mut config = if path.exists() {
        let file = std::fs::File::open(&path).context("Open native capability configuration")?;
        ensure!(
            file.metadata()?.len() <= CONFIG_BYTES,
            "Capability configuration exceeds 1MiB budget"
        );
        let mut bytes = Vec::new();
        file.take(CONFIG_BYTES + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= CONFIG_BYTES,
            "Capability configuration exceeds 1MiB budget"
        );
        serde_json::from_slice::<CapabilityConfig>(&bytes)
            .context("Read native capability configuration")?
    } else {
        CapabilityConfig::default()
    };
    // A file cannot redirect the helper's filesystem permission boundary.
    config.workspace = workspace.to_path_buf();
    config.artifacts = data.join("artifacts");
    config.management_root = Some(data.join("native-management"));
    Ok(config)
}

fn save_configuration(data: &Path, config: &CapabilityConfig) -> Result<()> {
    let target = data.join("capabilities.json");
    let temporary = data.join(format!("capabilities-{}.tmp", Uuid::new_v4()));
    let result = (|| {
        let bytes = serde_json::to_vec_pretty(config)?;
        ensure!(
            bytes.len() as u64 <= CONFIG_BYTES,
            "Capability configuration exceeds 1MiB budget"
        );
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, &target)?;
        #[cfg(unix)]
        std::fs::File::open(data)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

fn acquire_configuration_lock(data: &Path) -> Result<std::fs::File> {
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(data.join("capabilities.lock"))?;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match fs2::FileExt::try_lock_exclusive(&lock) {
            Ok(()) => break,
            Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {
                ensure!(
                    std::time::Instant::now() < deadline,
                    "Capability configuration is busy"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(lock)
}

/// The caller retains configuration and idle activity locks through this
/// transaction and replacement. Unknown top-level settings fail validation.
fn replace_configuration(
    data: &Path,
    workspace: &Path,
    provided: Option<(Value, bool)>,
) -> Result<CapabilityService> {
    let persist = provided.is_some();
    let mut config = match provided {
        Some((value, true)) => {
            let mut current = serde_json::to_value(load_configuration(data, workspace)?)?;
            let known = serde_json::to_value(CapabilityConfig::default())?;
            for key in value
                .as_object()
                .context("Configuration patch must be an object")?
                .keys()
            {
                ensure!(
                    known.get(key).is_some(),
                    "Unknown native capability setting: {key}"
                );
            }
            merge_config(&mut current, &value);
            serde_json::from_value(current)
                .context("Invalid native capability configuration patch")?
        }
        Some((value, false)) => {
            serde_json::from_value(value).context("Invalid native capability configuration")?
        }
        None => load_configuration(data, workspace)?,
    };
    config.workspace = workspace.to_path_buf();
    config.artifacts = data.join("artifacts");
    config.management_root = Some(data.join("native-management"));
    // Construction validates budgets/roots and starts no heavy native service.
    let replacement = CapabilityService::new(config.clone())?;
    if persist {
        save_configuration(data, &config)?;
    }
    Ok(replacement)
}

fn merge_config(target: &mut Value, patch: &Value) {
    if let (Some(target), Some(patch)) = (target.as_object_mut(), patch.as_object()) {
        for (key, value) in patch {
            if let Some(existing) = target.get_mut(key) {
                merge_config(existing, value);
            } else {
                target.insert(key.clone(), value.clone());
            }
        }
    } else {
        *target = patch.clone();
    }
}

async fn send(output: &mpsc::Sender<Response>, response: Response) -> Result<()> {
    let response = if protocol::encode(&response).is_ok() {
        response
    } else {
        Response::failed(
            response.id,
            "response_too_large",
            "Native capability response exceeds protocol budget",
        )
    };
    output
        .send(response)
        .await
        .context("Native helper output queue closed")
}

pub async fn serve(data: PathBuf, workspace: PathBuf) -> Result<()> {
    std::fs::create_dir_all(&data)?;
    let data = std::fs::canonicalize(data)?;
    let workspace = std::fs::canonicalize(workspace)?;
    ensure!(
        workspace.is_dir(),
        "Native helper workspace must be a directory"
    );
    std::fs::create_dir_all(data.join("models"))?;
    let config = load_configuration(&data, &workspace)?;
    let service = CapabilityService::new(config)?;
    let assets = AssetManager::new(&data)?;
    let audio = AudioService::new(&workspace, &data.join("artifacts"))?;
    let state = Arc::new(State {
        assets: assets.clone(),
        audio: audio.clone(),
        data,
        workspace,
        service: RwLock::new(service),
    });
    let transport_stopped = CancellationToken::new();
    let writer_stopped = transport_stopped.clone();
    let session_config = load_configuration(&state.data, &state.workspace)?;
    let (output, mut pending) = mpsc::channel::<Response>(QUEUE_CAPACITY);
    let writer = tokio::spawn(async move {
        let result = async {
            let mut stdout = BufWriter::new(tokio::io::stdout());
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut last_jobs = 0;
            loop {
                let mut response = tokio::select! {
                    response=pending.recv()=>match response{Some(response)=>response,None=>break},
                    _=interval.tick()=>{
                        if assets.active_jobs()+audio.active_jobs()+xylo_capabilities::environments::active_sessions(&session_config)==last_jobs{continue;}
                        Response::completed(Uuid::nil(),json!({"event":"background_jobs_changed"}))
                    }
                };
                response.background_jobs = assets.active_jobs()
                    + audio.active_jobs()
                    + xylo_capabilities::environments::active_sessions(&session_config);
                last_jobs = response.background_jobs;
                tokio::time::timeout(
                    Duration::from_secs(10),
                    protocol::write_frame(&mut stdout, &response),
                )
                .await
                .context("Helper output pipe timed out")??;
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        writer_stopped.cancel();
        result
    });

    // Reading has its own task: partial frames are never discarded when a tool
    // completion wins a select branch in the dispatch loop.
    let (commands, mut incoming) = mpsc::channel::<Request>(QUEUE_CAPACITY);
    let reader = tokio::spawn(async move {
        let mut stdin = BufReader::new(tokio::io::stdin());
        while let Some(frame) = protocol::read_frame(&mut stdin).await? {
            let request = serde_json::from_slice::<Request>(&frame)
                .context("Invalid native helper request")?;
            if commands.send(request).await.is_err() {
                break;
            }
        }
        Ok::<_, anyhow::Error>(())
    });

    let mut active: HashMap<Uuid, CancellationToken> = HashMap::new();
    let mut cancelled_before_dispatch = HashMap::<Uuid, std::time::Instant>::new();
    let mut task_ids = HashMap::new();
    let mut jobs = JoinSet::new();
    let mut protocol_error = None;
    loop {
        tokio::select! {
            _ = transport_stopped.cancelled() => {protocol_error=Some("Native helper output writer stopped");break;},
            completed = jobs.join_next_with_id(), if !jobs.is_empty() => {
                match completed {
                    Some(Ok((task_id,id))) => { active.remove(&id); task_ids.remove(&task_id); },
                    Some(Err(error)) => {
                        if let Some(id)=task_ids.remove(&error.id()) {
                            active.remove(&id);
                            send(&output,Response::failed(id,"operation_panicked","Native operation stopped unexpectedly; inspect artifacts before retrying any write")).await?;
                        }
                    },
                    None=>(),
                }
            },
            request = incoming.recv() => {
                let Some(request) = request else { break; };
                if request.protocol_version != PROTOCOL_VERSION {
                    send(&output, Response::failed(request.id,"protocol_version","Unsupported native helper protocol version")).await?;
                    continue;
                }
                if active.contains_key(&request.id) {
                    send(&output, Response::failed(request.id,"duplicate_request","Request ID is already active")).await?;
                    continue;
                }
                match request.command {
                    Command::Cancel { request_id } => {
                        let found = active.get(&request_id);
                        if let Some(cancel) = found { cancel.cancel(); }
                        else {
                            // The priority control pipe can overtake a queued
                            // execute frame. Retain a bounded tombstone so that
                            // cancelled queued writes never start afterward.
                            if cancelled_before_dispatch.len() >= QUEUE_CAPACITY * 2
                                && let Some(id)=cancelled_before_dispatch.iter().min_by_key(|(_,at)|*at).map(|(id,_)|*id) {cancelled_before_dispatch.remove(&id);}
                            cancelled_before_dispatch.insert(request_id,std::time::Instant::now());
                        }
                        send(&output, Response::completed(request.id,json!({"cancellation_requested":found.is_some(),"request_id":request_id}))).await?;
                    },
                    Command::Shutdown => {
                        send(&output, Response::completed(request.id,json!({"state":"stopping"}))).await?;
                        break;
                    },
                    Command::Execute { name, args } => {
                        if cancelled_before_dispatch.remove(&request.id).is_some() {
                            send(&output,Response::failed(request.id,"cancelled","Native helper request cancelled before dispatch")).await?;
                            continue;
                        }
                        if name.is_empty() || name.len()>128 || !name.chars().all(|c|c.is_ascii_alphanumeric()||c=='_') {
                            send(&output,Response::failed(request.id,"invalid_command","Invalid native helper command name")).await?;
                            continue;
                        }
                        let audio_control=matches!(name.as_str(),"helper_audio_record_stop"|"helper_audio_record_cancel"|"helper_audio_play_stop");
                        if active.len()>=QUEUE_CAPACITY+if audio_control{8}else{0} {
                            send(&output,Response::failed(request.id,"busy","Native helper has 64 active requests")).await?;
                            continue;
                        }
                        if matches!(name.as_str(),"helper_configure"|"helper_patch_config"|"helper_reload_config"|"helper_release_resources") {
                            for cancel in active.values() { cancel.cancel(); }
                        }
                        let cancel=CancellationToken::new();
                        active.insert(request.id,cancel.clone());
                        let state=state.clone();
                        let output=output.clone();
                        let handle=jobs.spawn(async move {
                            let result=state.execute(&name,&args,cancel).await;
                            let response=match result {
                                Ok(value)=>Response::completed(request.id,value),
                                Err(error)=>Response::failed(request.id,"operation_failed",format!("{error:#}")),
                            };
                            let _=send(&output,response).await;
                            request.id
                        });
                        task_ids.insert(handle.id(),request.id);
                    },
                }
            },
        }
    }
    for cancel in active.values() {
        cancel.cancel();
    }
    let drain = async { while jobs.join_next().await.is_some() {} };
    if tokio::time::timeout(Duration::from_secs(3), drain)
        .await
        .is_err()
    {
        jobs.abort_all();
        while jobs.join_next().await.is_some() {}
    }
    state.audio.shutdown().await;
    state.assets.shutdown().await;
    let release = tokio::time::timeout(
        Duration::from_secs(3),
        state.service.write().await.shutdown(),
    )
    .await;
    if !reader.is_finished() {
        reader.abort();
    }
    let reader_result = reader.await;
    drop(output);
    tokio::time::timeout(Duration::from_secs(3), writer)
        .await
        .context("Helper output shutdown timed out")???;
    if let Some(error) = protocol_error {
        anyhow::bail!(error);
    }
    if let Ok(result) = reader_result {
        result?;
    }
    release.context("Native capability cleanup timed out")??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn configuration_cannot_replace_authorized_roots_and_save_is_atomic() {
        let data = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let config = CapabilityConfig {
            workspace: PathBuf::from("/untrusted"),
            artifacts: PathBuf::from("/untrusted"),
            management_root: Some(PathBuf::from("/untrusted/management")),
            ..Default::default()
        };
        save_configuration(data.path(), &config).unwrap();
        let loaded = load_configuration(data.path(), workspace.path()).unwrap();
        assert_eq!(loaded.workspace, workspace.path());
        assert_eq!(loaded.artifacts, data.path().join("artifacts"));
        assert_eq!(
            loaded.management_root,
            Some(data.path().join("native-management"))
        );
        assert!(std::fs::read_dir(data.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        }));
    }

    #[test]
    fn configuration_patch_cannot_redirect_management_storage() {
        let data = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let redirected = workspace.path().join("redirected");
        replace_configuration(
            data.path(),
            workspace.path(),
            Some((json!({"management_root": redirected}), true)),
        )
        .unwrap();
        let loaded = load_configuration(data.path(), workspace.path()).unwrap();
        assert_eq!(
            loaded.management_root,
            Some(data.path().join("native-management"))
        );
        assert!(!redirected.exists());
        assert!(data.path().join("native-management").is_dir());
    }
}
