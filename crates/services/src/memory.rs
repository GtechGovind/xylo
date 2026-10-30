use crate::Store;
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use fastembed::{
    InitOptionsUserDefined, Pooling, TextEmbedding, TokenizerFiles, UserDefinedEmbeddingModel,
};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::time::{Duration, Instant};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use uuid::Uuid;
use xylo_core::{MemoryBackend, MemoryRecord};

/// Pooling is part of model compatibility, not an interchangeable tuning choice.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingPooling {
    #[default]
    Auto,
    Cls,
    Mean,
}
/// Explicit local text-model assets. No model or runtime loads until first use.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingAssets {
    pub directory: PathBuf,
    pub model_id: String,
    pub dimensions: usize,
    /// Zero follows current CPU availability; positive values are upper bounds.
    #[serde(default)]
    pub cpu_threads: usize,
    #[serde(default)]
    pub pooling: EmbeddingPooling,
    /// Defaults to the platform ONNX Runtime library inside directory.
    #[serde(default)]
    pub runtime_library: Option<PathBuf>,
}
impl EmbeddingAssets {
    fn threads_for_available(&self, available: usize) -> usize {
        let available = available.saturating_sub(1).max(1);
        if self.cpu_threads == 0 {
            available
        } else {
            self.cpu_threads.min(available)
        }
    }
    pub fn runtime_filename() -> &'static str {
        #[cfg(target_os = "macos")]
        {
            "libonnxruntime.dylib"
        }
        #[cfg(target_os = "windows")]
        {
            "onnxruntime.dll"
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            "libonnxruntime.so"
        }
    }
    fn resolved_pooling(&self) -> Result<Pooling> {
        Ok(match self.pooling {
            EmbeddingPooling::Cls => Pooling::Cls,
            EmbeddingPooling::Mean => Pooling::Mean,
            EmbeddingPooling::Auto
                if self
                    .model_id
                    .to_ascii_lowercase()
                    .contains("bge-small-en-v1.5") =>
            {
                Pooling::Cls
            }
            EmbeddingPooling::Auto if self.model_id.to_ascii_lowercase().contains("all-minilm") => {
                Pooling::Mean
            }
            EmbeddingPooling::Auto => {
                anyhow::bail!("Unknown embedding model pooling; select explicit cls or mean")
            }
        })
    }
    /// Pooling and dimensionality change the vector space even when weights match.
    fn index_id(&self) -> Result<String> {
        let pooling = match self.resolved_pooling()? {
            Pooling::Cls => "cls",
            Pooling::Mean => "mean",
        };
        Ok(format!("{}:{}:{pooling}", self.model_id, self.dimensions))
    }
}
struct Embedder {
    assets: EmbeddingAssets,
    model: Option<TextEmbedding>,
    last_used: Option<Instant>,
}
impl Embedder {
    fn embed(&mut self, text: &str) -> Result<Vec<f32>> {
        if self.model.is_none() {
            let library = self
                .assets
                .runtime_library
                .as_ref()
                .context("Missing configured ONNX Runtime library")?;
            let environment = ort::init_from(library)
                .map_err(|e| anyhow::anyhow!("Load local ONNX Runtime library: {e}"))?
                .with_name("Xylo memory")
                .with_telemetry(false)
                .build()
                .context("Initialize local embedding runtime")?;
            let read = |name: &str| {
                std::fs::read(self.assets.directory.join(name))
                    .with_context(|| format!("Missing local embedding asset {name}"))
            };
            let model = UserDefinedEmbeddingModel::new(
                read("model.onnx")?,
                TokenizerFiles {
                    tokenizer_file: read("tokenizer.json")?,
                    config_file: read("config.json")?,
                    special_tokens_map_file: read("special_tokens_map.json")?,
                    tokenizer_config_file: read("tokenizer_config.json")?,
                },
            )
            .with_pooling(self.assets.resolved_pooling()?);
            self.model = Some(TextEmbedding::try_new_from_user_defined(
                model,
                InitOptionsUserDefined::new()
                    .with_environment(environment)
                    .with_max_length(512)
                    .with_intra_threads(self.assets.threads_for_available(
                        std::thread::available_parallelism().map_or(1, usize::from),
                    )),
            )?);
        }
        let mut output = self
            .model
            .as_mut()
            .context("Embedding session not initialized")?
            .embed([text], Some(1))?;
        let vector = output
            .pop()
            .context("Embedding model returned no vectors")?;
        ensure!(
            vector.len() == self.assets.dimensions && vector.iter().all(|v| v.is_finite()),
            "Embedding model dimensions/output invalid"
        );
        self.last_used = Some(Instant::now());
        Ok(vector)
    }
}
#[derive(Clone)]
pub struct SqliteMemory {
    store: Store,
    embedder: Arc<Mutex<Option<Embedder>>>,
}
impl SqliteMemory {
    pub(crate) fn store(&self) -> &Store {
        &self.store
    }
    pub fn new(store: Store) -> Self {
        Self {
            store,
            embedder: Arc::new(Mutex::new(None)),
        }
    }
    /// Missing config is ordinary lexical mode. Existing invalid configuration is
    /// an explicit stable error, not silent fallback or a download trigger.
    pub fn configured(store: Store, config_path: &std::path::Path) -> Result<Self> {
        let memory = Self::new(store);
        let file = match std::fs::File::open(config_path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(memory),
            Err(error) => return Err(error).context("Open embedding configuration"),
        };
        let mut bytes = Vec::new();
        file.take(65537)
            .read_to_end(&mut bytes)
            .context("Read embedding configuration")?;
        ensure!(
            bytes.len() <= 65536,
            "Embedding configuration exceeds 64 KiB"
        );
        let mut assets: EmbeddingAssets =
            serde_json::from_slice(&bytes).context("Invalid embedding configuration JSON")?;
        if assets.directory.is_relative() {
            assets.directory = config_path
                .parent()
                .context("Embedding configuration has no parent directory")?
                .join(&assets.directory);
        }
        memory.enable_embeddings(assets)?;
        Ok(memory)
    }
    /// Validate a replacement before swapping the shared lazy session. Run on a
    /// blocking worker: an active inference may finish before the swap can occur.
    /// Invalid metadata leaves the current session/configuration intact.
    pub fn reload_configuration(&self, config_path: &std::path::Path) -> Result<()> {
        let replacement = Self::configured(self.store.clone(), config_path)?;
        let replacement = replacement
            .embedder
            .lock()
            .map_err(|_| anyhow::anyhow!("Embedding lock poisoned"))?
            .take();
        *self
            .embedder
            .lock()
            .map_err(|_| anyhow::anyhow!("Embedding lock poisoned"))? = replacement;
        Ok(())
    }
    /// Drop a resident model while retaining its validated metadata for lazy reload.
    /// A busy inference is not blocked or interrupted by an idle maintenance pass.
    pub fn unload_if_idle(&self, idle: Duration) -> Result<bool> {
        let mut guard = match self.embedder.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::WouldBlock) => return Ok(false),
            Err(error) => return Err(anyhow::anyhow!("Embedding lock poisoned: {error}")),
        };
        if let Some(embedder) = guard.as_mut()
            && embedder.model.is_some()
            && embedder
                .last_used
                .is_some_and(|used| used.elapsed() >= idle)
        {
            embedder.model = None;
            return Ok(true);
        }
        Ok(false)
    }
    /// Validates metadata and paths only. DLL/model loading remains lazy.
    pub fn enable_embeddings(&self, mut assets: EmbeddingAssets) -> Result<()> {
        ensure!(
            !assets.model_id.is_empty()
                && assets.dimensions > 0
                && assets.dimensions <= 4096
                && assets.cpu_threads <= 4096,
            "Invalid embedding configuration"
        );
        ensure!(
            assets.directory.is_absolute(),
            "Embedding asset directory must be absolute"
        );
        assets.resolved_pooling()?;
        let runtime = assets
            .runtime_library
            .take()
            .unwrap_or_else(|| PathBuf::from(EmbeddingAssets::runtime_filename()));
        let runtime = if runtime.is_relative() {
            assets.directory.join(runtime)
        } else {
            runtime
        };
        ensure!(
            runtime.is_file(),
            "Missing local ONNX Runtime library {}",
            runtime.display()
        );
        assets.runtime_library = Some(runtime);
        for name in [
            "model.onnx",
            "tokenizer.json",
            "config.json",
            "special_tokens_map.json",
            "tokenizer_config.json",
        ] {
            ensure!(
                assets.directory.join(name).is_file(),
                "Missing local embedding asset {name}"
            );
        }
        *self
            .embedder
            .lock()
            .map_err(|_| anyhow::anyhow!("Embedding lock poisoned"))? = Some(Embedder {
            assets,
            model: None,
            last_used: None,
        });
        Ok(())
    }
    pub fn disable_embeddings(&self) -> Result<()> {
        *self
            .embedder
            .lock()
            .map_err(|_| anyhow::anyhow!("Embedding lock poisoned"))? = None;
        Ok(())
    }
    fn vector(&self, text: &str) -> Result<Option<(String, Vec<u8>)>> {
        let mut guard = self
            .embedder
            .lock()
            .map_err(|_| anyhow::anyhow!("Embedding lock poisoned"))?;
        guard
            .as_mut()
            .map(|e| {
                let v = e.embed(text)?;
                Ok((
                    e.assets.index_id()?,
                    v.iter().flat_map(|n| n.to_le_bytes()).collect(),
                ))
            })
            .transpose()
    }
    pub fn remember_sync(&self, record: MemoryRecord) -> Result<()> {
        ensure!(
            !record.scope.trim().is_empty()
                && !record.content.trim().is_empty()
                && record.content.len() <= 262_144
                && record.scope.len() <= 8192
                && record.source.len() <= 8192
                && !record.scope.contains('\0'),
            "Memory requires scope and bounded content"
        );
        ensure!(
            record.supersedes != Some(record.id),
            "Memory cannot supersede itself"
        );
        // Idempotent retries do not load the embedding runtime or reactivate a
        // superseded version. A reused ID with changed content is rejected.
        let existing = record.clone();
        let (exists, generation) = self.store.call(move |conn| {
            crate::memory_facts::initialize(conn)?;
            crate::memory_facts::reject_deleted(conn, existing.id)?;
            Ok((
                same_record(conn, &existing)?,
                crate::memory_facts::generation(conn, &existing.scope)?,
            ))
        })?;
        if exists {
            return Ok(());
        }
        let vector = self.vector(&record.content).unwrap_or_else(|_| {
            tracing::warn!("Optional memory embedding unavailable; saving lexical memory");
            None
        });
        self.store.call(move|conn|{let tx=conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            crate::memory_facts::reject_deleted(&tx,record.id)?;
            if same_record(&tx, &record)? { tx.commit()?; return Ok(()); }
            ensure!(crate::memory_facts::generation(&tx,&record.scope)?==generation,"Project facts changed during this save; refresh and retry");
            if let Some(previous)=record.supersedes {let previous_state:Option<(String,bool)>=tx.query_row("SELECT scope,active FROM memories WHERE id=?1",[previous.to_string()],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;ensure!(previous_state.as_ref().is_some_and(|(scope,active)|scope==&record.scope&&*active),"Correct the current memory in the same project");tx.execute("UPDATE memories SET active=0 WHERE id=?1",[previous.to_string()])?; delete_vectors(&tx, &previous.to_string())?;}
            let (mut model,mut embedding)=match vector{Some((m,v))=>(Some(m),Some(v)),None=>(None,None)};
            if let (Some(vector_model), Some(vector_embedding)) = (&model, &embedding) {
                tx.execute_batch("SAVEPOINT optional_memory_vector")?;
                let indexed = (|| -> Result<()> { let table = ensure_vector_table(&tx, vector_model, vector_embedding.len()/4)?; tx.execute(&format!("INSERT INTO {table}(id,scope,embedding) VALUES(?1,?2,?3)"),params![record.id.to_string(),record.scope,vector_embedding])?; Ok(()) })();
                if indexed.is_err() { tx.execute_batch("ROLLBACK TO optional_memory_vector")?; model = None; embedding = None; tracing::warn!("Optional semantic index unavailable; saving lexical memory"); }
                tx.execute_batch("RELEASE optional_memory_vector")?;
            }
            tx.execute("INSERT INTO memories(id,scope,content,json,embedding,embedding_model) VALUES(?1,?2,?3,?4,?5,?6)",params![record.id.to_string(),record.scope,record.content,serde_json::to_string(&record)?,embedding,model])?;
            tx.execute("INSERT INTO memories_fts(id,scope,content) VALUES(?1,?2,?3)",params![record.id.to_string(),record.scope,record.content])?;
            crate::memory_facts::inserted(&tx,&record)?;tx.commit()?;Ok(())})
    }
    pub fn recall_sync(&self, scope: &str, query: &str, limit: usize) -> Result<Vec<MemoryRecord>> {
        if limit == 0 || query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let limit = limit.min(100);
        let scope = scope.to_owned();
        let terms: Vec<_> = query
            .split_whitespace()
            .take(32)
            .map(|s| format!("\"{}\"", s.replace('"', "\"\"")))
            .collect();
        let fts = terms.join(" OR ");
        let vector = self.vector(query).unwrap_or_else(|_| {
            tracing::warn!("Optional memory embedding unavailable; using lexical recall");
            None
        });
        self.store.call(move|conn|{
            let mut lexical=conn.prepare("SELECT m.json FROM memories_fts f JOIN memories m ON m.id=f.id WHERE memories_fts MATCH ?1 AND m.scope=?2 AND m.active=1 ORDER BY bm25(memories_fts) LIMIT ?3")?;
            let lexical=lexical.query_map(params![fts,scope,(limit*4) as i64],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            let mut ranked:HashMap<Uuid,(f64,MemoryRecord)>=HashMap::new();
            let mut add=|items:Vec<String>|->Result<()>{for (rank,json) in items.into_iter().enumerate(){let record:MemoryRecord=serde_json::from_str(&json)?;ranked.entry(record.id).and_modify(|(score,_)|*score+=1.0/(60.0+rank as f64)).or_insert((1.0/(60.0+rank as f64),record));}Ok(())};
            add(lexical)?;
            if let Some((model,embedding))=vector {
                let semantic_result = (|| -> Result<Vec<String>> {
                    let table = ensure_vector_table(conn, &model, embedding.len()/4)?;
                    let mut semantic=conn.prepare(&format!("SELECT m.json FROM {table} v JOIN memories m ON m.id=v.id WHERE v.embedding MATCH ?1 AND k=?2 AND v.scope=?3 AND m.active=1 ORDER BY v.distance"))?;
                    Ok(semantic.query_map(params![embedding,(limit*4).min(4096) as i64,scope],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?)
                })();
                match semantic_result { Ok(items) => add(items.into_iter().filter(|json| serde_json::from_str::<MemoryRecord>(json).is_ok()).collect())?, Err(_) => tracing::warn!("Optional semantic index unavailable; using lexical recall") }

            }
            let mut ranked:Vec<_>=ranked.into_values().collect();ranked.sort_by(|a,b|b.0.total_cmp(&a.0).then_with(||b.1.created_at.cmp(&a.1.created_at)));Ok(ranked.into_iter().take(limit).map(|(_,record)|record).collect())})
    }
    pub fn forget_sync(&self, id: Uuid) -> Result<()> {
        self.store.call(move |conn| {
            crate::memory_facts::initialize(conn)?;
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let scope: Option<String> = tx
                .query_row(
                    "SELECT scope FROM memory_fact_links WHERE id=?1",
                    [id.to_string()],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(scope) = scope {
                crate::memory_facts::delete_selected(&tx, &scope, id)?;
            }
            tx.commit()?;
            Ok(())
        })
    }
    /// Deletes only this selected version. Other versions are not reactivated.
    pub fn forget_scoped(&self, scope: &str, id: Uuid) -> Result<()> {
        crate::memory_facts::validate_scope(scope)?;
        let scope = scope.to_owned();
        self.store.call(move |conn| {
            crate::memory_facts::initialize(conn)?;
            let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            crate::memory_facts::delete_selected(&tx, &scope, id)?;
            tx.commit()?;
            Ok(())
        })
    }
    pub fn reindex(&self, scope: &str, limit: usize) -> Result<usize> {
        let model = {
            let guard = self
                .embedder
                .lock()
                .map_err(|_| anyhow::anyhow!("Embedding lock poisoned"))?;
            guard
                .as_ref()
                .context("Enable embeddings before reindexing")?
                .assets
                .index_id()?
        };
        let scope = scope.to_owned();
        let model_query = model.clone();
        let records:Vec<(String,String)>=self.store.call(move|conn|{let mut stmt=conn.prepare("SELECT id,content FROM memories WHERE scope=?1 AND active=1 AND (embedding_model IS NULL OR embedding_model<>?2) LIMIT ?3")?;let rows=stmt.query_map(params![scope,model_query,limit.min(1000) as i64],|r|Ok((r.get(0)?,r.get(1)?)))?;Ok(rows.collect::<rusqlite::Result<_>>()?)})?;
        let count = records.len();
        for (id, content) in records {
            let (model, vector) = self
                .vector(&content)?
                .context("Embedding configuration was disabled during reindex")?;
            self.store.call(move |conn| {
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let scope: String =
                    tx.query_row("SELECT scope FROM memories WHERE id=?1", [&id], |r| {
                        r.get(0)
                    })?;
                delete_vectors(&tx, &id)?;
                let table = ensure_vector_table(&tx, &model, vector.len() / 4)?;
                tx.execute(
                    &format!("INSERT INTO {table}(id,scope,embedding) VALUES(?1,?2,?3)"),
                    params![id, scope, vector],
                )?;
                tx.execute(
                    "UPDATE memories SET embedding=?1,embedding_model=?2 WHERE id=?3",
                    params![vector, model, id],
                )?;
                tx.commit()?;
                Ok(())
            })?;
        }
        Ok(count)
    }
}
#[async_trait]
impl MemoryBackend for SqliteMemory {
    async fn remember(&self, record: MemoryRecord) -> Result<()> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.remember_sync(record)).await?
    }
    async fn recall(&self, scope: &str, query: &str, limit: usize) -> Result<Vec<MemoryRecord>> {
        let this = self.clone();
        let scope = scope.to_owned();
        let query = query.to_owned();
        tokio::task::spawn_blocking(move || this.recall_sync(&scope, &query, limit)).await?
    }
    async fn forget(&self, id: Uuid) -> Result<()> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.forget_sync(id)).await?
    }
}

// Names are derived exclusively from a SHA-256 digest, never user-provided SQL.
fn ensure_vector_table(
    conn: &rusqlite::Connection,
    model: &str,
    dimensions: usize,
) -> Result<String> {
    ensure!(
        dimensions > 0 && dimensions <= 4096,
        "Invalid vector dimensions"
    );
    let hash = format!("{:x}", Sha256::digest(format!("{model}:{dimensions}")));
    let table = format!("memory_vec_{}", &hash[..24]);
    conn.execute_batch("CREATE TABLE IF NOT EXISTS memory_vector_tables(name TEXT PRIMARY KEY,model TEXT NOT NULL,dimensions INTEGER NOT NULL)")?;
    conn.execute_batch(&format!("CREATE VIRTUAL TABLE IF NOT EXISTS {table} USING vec0(id TEXT PRIMARY KEY,scope TEXT PARTITION KEY,embedding float[{dimensions}] distance_metric=cosine)"))?;
    conn.execute(
        "INSERT OR IGNORE INTO memory_vector_tables(name,model,dimensions) VALUES(?1,?2,?3)",
        params![table, model, dimensions as i64],
    )?;
    Ok(table)
}
pub(crate) fn delete_vectors(conn: &rusqlite::Connection, id: &str) -> Result<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS memory_vector_tables(name TEXT PRIMARY KEY,model TEXT NOT NULL,dimensions INTEGER NOT NULL)")?;
    let tables = conn
        .prepare("SELECT name FROM memory_vector_tables")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for table in tables {
        ensure!(
            table.starts_with("memory_vec_") && table[11..].bytes().all(|b| b.is_ascii_hexdigit()),
            "Invalid persisted vector index name"
        );
        conn.execute(&format!("DELETE FROM {table} WHERE id=?1"), [id])?;
    }
    Ok(())
}

pub(crate) fn delete_vector_targets(conn: &rusqlite::Connection) -> Result<()> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='memory_vector_tables')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(());
    }
    let tables = conn
        .prepare("SELECT name FROM memory_vector_tables")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for table in tables {
        ensure!(
            table.starts_with("memory_vec_") && table[11..].bytes().all(|b| b.is_ascii_hexdigit()),
            "Invalid persisted vector index name"
        );
        conn.execute(
            &format!("DELETE FROM {table} WHERE id IN(SELECT id FROM memory_delete_targets)"),
            [],
        )?;
    }
    Ok(())
}

// Canonical JSON includes provenance and supersession; a retry is identical only
// when every app-owned field matches, including timestamps and scope.
fn same_record(conn: &rusqlite::Connection, record: &MemoryRecord) -> Result<bool> {
    let existing: Option<String> = conn
        .query_row(
            "SELECT json FROM memories WHERE id=?1",
            [record.id.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(json) = existing {
        ensure!(
            serde_json::from_str::<serde_json::Value>(&json)? == serde_json::to_value(record)?,
            "Memory ID was reused for different content"
        );
        return Ok(true);
    }
    Ok(false)
}

#[cfg(test)]
mod vector_tests {
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
        let memory =
            SqliteMemory::configured(store, &directory.path().join("embedding.json")).unwrap();
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
}
