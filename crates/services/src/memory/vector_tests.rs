use super::*;

fn metadata_assets(directory: &std::path::Path) -> EmbeddingAssets {
    EmbeddingAssets {
        directory: directory.to_owned(),
        model_id: "BAAI/bge-small-en-v1.5".into(),
        dimensions: 384,
        cpu_threads: 1,
        pooling: EmbeddingPooling::Auto,
        runtime_library: None,
    }
}

#[test]
fn automatic_threads_follow_available_cpus_and_explicit_values_remain_bounds() {
    let mut assets = metadata_assets(std::path::Path::new("/unused"));
    assets.cpu_threads = 0;
    assert_eq!(assets.threads_for_available(1), 1);
    assert_eq!(assets.threads_for_available(32), 31);
    assets.cpu_threads = 24;
    assert_eq!(assets.threads_for_available(8), 7);
    assert_eq!(assets.threads_for_available(64), 24);
    let mut json = serde_json::to_value(&assets).unwrap();
    json.as_object_mut().unwrap().remove("cpu_threads");
    assert_eq!(
        serde_json::from_value::<EmbeddingAssets>(json)
            .unwrap()
            .cpu_threads,
        0
    );
}

fn write_metadata_fixture(directory: &std::path::Path) {
    std::fs::create_dir_all(directory).unwrap();
    for file in [
        "model.onnx",
        "tokenizer.json",
        "config.json",
        "special_tokens_map.json",
        "tokenizer_config.json",
        EmbeddingAssets::runtime_filename(),
    ] {
        // Deliberately invalid assets prove that configuration does not load them.
        std::fs::write(directory.join(file), b"metadata fixture only").unwrap();
    }
}

#[test]
fn configured_missing_file_keeps_lexical_memory_available() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("missing-config.db")).unwrap();
    let memory = SqliteMemory::configured(store, &directory.path().join("embedding.json")).unwrap();
    assert!(memory.embedder.lock().unwrap().is_none());
    assert!(
        memory
            .recall_sync("scope", "anything", 5)
            .unwrap()
            .is_empty()
    );
    assert!(!memory.unload_if_idle(Duration::ZERO).unwrap());
}

#[test]
fn broken_optional_runtime_keeps_lexical_save_and_recall_available() {
    let directory = tempfile::tempdir().unwrap();
    let assets_directory = directory.path().join("assets");
    write_metadata_fixture(&assets_directory);
    let config = directory.path().join("embedding.json");
    std::fs::write(
        &config,
        serde_json::to_vec(&metadata_assets(&assets_directory)).unwrap(),
    )
    .unwrap();
    let store = Store::open(directory.path().join("fallback.db")).unwrap();
    let memory = SqliteMemory::configured(store, &config).unwrap();
    let record = xylo_core::MemoryRecord {
        id: Uuid::new_v4(),
        scope: "project-a".into(),
        content: "Prefer violet accent".into(),
        source: "Explicit user choice".into(),
        created_at: 7,
        supersedes: None,
    };
    memory.remember_sync(record.clone()).unwrap();
    assert_eq!(
        memory.recall_sync("project-a", "violet", 3).unwrap()[0].id,
        record.id
    );
    let EmbeddingStatus::Failed { message } = memory.embedding_status() else {
        panic!("the load failure is recorded");
    };
    assert_eq!(memory.take_embedding_failure(), Some(message.clone()));
    assert_eq!(memory.take_embedding_failure(), None, "reported once");
    let failed_at = memory.failure.lock().unwrap().as_ref().unwrap().at;
    memory.recall_sync("project-a", "violet", 3).unwrap();
    assert_eq!(
        memory.failure.lock().unwrap().as_ref().unwrap().at,
        failed_at,
        "no reload is attempted during the retry delay"
    );
    memory.disable_embeddings().unwrap();
    assert_eq!(memory.embedding_status(), EmbeddingStatus::Disabled);
}

#[test]
fn configured_validates_metadata_and_resolves_paths_without_loading_assets() {
    let directory = tempfile::tempdir().unwrap();
    let assets_directory = directory.path().join("assets");
    write_metadata_fixture(&assets_directory);
    let mut assets = metadata_assets(std::path::Path::new("assets"));
    let config = directory.path().join("embedding.json");
    std::fs::write(&config, serde_json::to_vec(&assets).unwrap()).unwrap();
    let store = Store::open(directory.path().join("configured.db")).unwrap();
    let memory = SqliteMemory::configured(store, &config).unwrap();
    {
        let guard = memory.embedder.lock().unwrap();
        let configured = guard.as_ref().unwrap();
        assert_eq!(configured.assets.directory, assets_directory);
        assert_eq!(
            configured.assets.runtime_library,
            Some(assets_directory.join(EmbeddingAssets::runtime_filename()))
        );
        assert!(matches!(
            configured.assets.resolved_pooling().unwrap(),
            Pooling::Cls
        ));
        assert!(configured.model.is_none());
        assert!(configured.last_used.is_none());
        assert!(!memory.unload_if_idle(Duration::ZERO).unwrap());
    }
    assert!(!memory.unload_if_idle(Duration::ZERO).unwrap());
    assets.dimensions = 0;
    std::fs::write(&config, serde_json::to_vec(&assets).unwrap()).unwrap();
    let store = Store::open(directory.path().join("invalid-metadata.db")).unwrap();
    let error = SqliteMemory::configured(store, &config).err().unwrap();
    assert_eq!(error.to_string(), "Invalid embedding configuration");
}

#[test]
fn configured_rejects_malformed_oversized_or_unrecognized_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("embedding.json");
    let store = Store::open(directory.path().join("malformed.db")).unwrap();
    std::fs::write(&config, b"{").unwrap();
    let error = SqliteMemory::configured(store.clone(), &config)
        .err()
        .unwrap();
    assert_eq!(error.to_string(), "Invalid embedding configuration JSON");
    std::fs::write(&config, vec![b' '; 65537]).unwrap();
    let error = SqliteMemory::configured(store.clone(), &config)
        .err()
        .unwrap();
    assert_eq!(error.to_string(), "Embedding configuration exceeds 64 KiB");
    let mut json = serde_json::to_value(metadata_assets(directory.path())).unwrap();
    json["download_models"] = serde_json::json!(true);
    std::fs::write(&config, serde_json::to_vec(&json).unwrap()).unwrap();
    let error = SqliteMemory::configured(store, &config).err().unwrap();
    assert_eq!(error.to_string(), "Invalid embedding configuration JSON");
}

#[test]
fn reload_validates_before_replacing_the_shared_lazy_configuration() {
    let directory = tempfile::tempdir().unwrap();
    let assets_directory = directory.path().join("assets");
    write_metadata_fixture(&assets_directory);
    let config = directory.path().join("embedding.json");
    std::fs::write(
        &config,
        serde_json::to_vec(&metadata_assets(&assets_directory)).unwrap(),
    )
    .unwrap();
    let memory = SqliteMemory::new(Store::open(directory.path().join("reload.db")).unwrap());
    let other = memory.clone();
    memory.reload_configuration(&config).unwrap();
    assert!(
        other
            .embedder
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .model
            .is_none()
    );
    std::fs::write(&config, b"not json").unwrap();
    assert!(memory.reload_configuration(&config).is_err());
    assert!(other.embedder.lock().unwrap().is_some());
    std::fs::remove_file(&config).unwrap();
    memory.reload_configuration(&config).unwrap();
    assert!(other.embedder.lock().unwrap().is_none());
}

#[test]
fn pooling_defaults_match_known_models_and_vector_spaces_do_not_mix() {
    let mut assets = metadata_assets(std::path::Path::new("/unused"));
    let cls_index = assets.index_id().unwrap();
    assert!(matches!(assets.resolved_pooling().unwrap(), Pooling::Cls));
    assets.pooling = EmbeddingPooling::Mean;
    assert!(matches!(assets.resolved_pooling().unwrap(), Pooling::Mean));
    assert_ne!(assets.index_id().unwrap(), cls_index);
    assets.pooling = EmbeddingPooling::Auto;
    assets.model_id = "sentence-transformers/all-MiniLM-L6-v2".into();
    assert!(matches!(assets.resolved_pooling().unwrap(), Pooling::Mean));
    assets.model_id = "my-custom-model".into();
    assert!(assets.resolved_pooling().is_err());
    assets.pooling = EmbeddingPooling::Cls;
    assert!(assets.resolved_pooling().is_ok());
}

#[test]
fn vec0_knn_uses_scope_partition_and_forget_removes_index_row() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("vectors.db")).unwrap();
    store.call(|conn|{
        let table=ensure_vector_table(conn,"fixture-model",3)?;
        for (id,scope,vector) in [("a","project-a",[1.0_f32,0.0,0.0]),("b","project-b",[1.0,0.0,0.0]),("c","project-a",[0.0,1.0,0.0])]{let blob:Vec<_>=vector.iter().flat_map(|n|n.to_le_bytes()).collect();conn.execute(&format!("INSERT INTO {table}(id,scope,embedding) VALUES(?1,?2,?3)"),params![id,scope,blob])?;}
        let query:Vec<_>=[1.0_f32,0.0,0.0].iter().flat_map(|n|n.to_le_bytes()).collect();
        let id:String=conn.query_row(&format!("SELECT id FROM {table} WHERE embedding MATCH ?1 AND k=1 AND scope='project-a' ORDER BY distance"),[query],|r|r.get(0))?;assert_eq!(id,"a");
        delete_vectors(conn,"a")?;let count:i64=conn.query_row(&format!("SELECT count(*) FROM {table} WHERE id='a'"),[],|r|r.get(0))?;assert_eq!(count,0);Ok(())
    }).unwrap();
}
