//! Project facts: hybrid lexical (FTS5 BM25) and optional semantic recall,
//! fused by reciprocal rank.
//!
//! Writes are non-destructive and provenance-bound. Every saved record is
//! redacted of credentials first ([`redaction`]), carries the origin the
//! application assigned it, and a correction (`supersedes`) deactivates the
//! version it replaces without deleting it, so recall can show what a fact
//! said before. A correction may come only from the same or a more trusted
//! origin tier ([`MemoryOrigin::tier`]).
mod redaction;

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
use xylo_core::{MemoryBackend, MemoryOrigin, MemoryRecord};

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
/// After a failure, model loading and inference are not retried for this long,
/// so a broken runtime does not cost a load attempt on every memory call.
const EMBEDDING_RETRY_AFTER: Duration = Duration::from_secs(10 * 60);

/// Whether semantic (embedding) memory is working. Lexical memory always works.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum EmbeddingStatus {
    /// No embedding model is configured.
    Disabled,
    /// Configured; the model loads on first use.
    Ready,
    /// The last load or inference failed; memory runs lexical-only until the
    /// retry delay passes or the configuration changes.
    Failed { message: String },
}

struct EmbeddingFailure {
    message: String,
    at: Instant,
    reported: bool,
}

/// A recalled fact and the version it corrected, if that still exists.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecalledFact {
    #[serde(flatten)]
    pub record: MemoryRecord,
    pub previously: Option<PreviousVersion>,
}

/// An inactive earlier version of a fact.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PreviousVersion {
    pub id: Uuid,
    pub content: String,
    pub origin: MemoryOrigin,
    pub created_at: i64,
    /// When the correction replaced it: the correction's `created_at`.
    pub until: i64,
}

#[derive(Clone)]
pub struct SqliteMemory {
    store: Store,
    embedder: Arc<Mutex<Option<Embedder>>>,
    /// Kept apart from `embedder` so status reads never wait on an inference.
    failure: Arc<Mutex<Option<EmbeddingFailure>>>,
}
impl SqliteMemory {
    pub(crate) fn store(&self) -> &Store {
        &self.store
    }
    pub fn new(store: Store) -> Self {
        Self {
            store,
            embedder: Arc::new(Mutex::new(None)),
            failure: Arc::new(Mutex::new(None)),
        }
    }
    /// Current state of semantic memory, for status and settings surfaces.
    pub fn embedding_status(&self) -> EmbeddingStatus {
        if let Ok(failure) = self.failure.lock()
            && let Some(failure) = failure.as_ref()
        {
            return EmbeddingStatus::Failed {
                message: failure.message.clone(),
            };
        }
        match self.embedder.try_lock() {
            Ok(guard) if guard.is_none() => EmbeddingStatus::Disabled,
            _ => EmbeddingStatus::Ready,
        }
    }
    /// The newest embedding failure, returned only once so the host can tell
    /// the user a single time instead of on every memory call.
    pub fn take_embedding_failure(&self) -> Option<String> {
        let mut failure = self.failure.lock().ok()?;
        let failure = failure.as_mut()?;
        if failure.reported {
            return None;
        }
        failure.reported = true;
        Some(failure.message.clone())
    }
    fn record_failure(&self, error: &anyhow::Error) {
        let message = format!("Semantic memory is unavailable: {error:#}");
        let Ok(mut failure) = self.failure.lock() else {
            return;
        };
        let repeated = failure
            .as_ref()
            .is_some_and(|known| known.message == message);
        if !repeated {
            tracing::warn!("{message}; memory continues with lexical search");
        }
        *failure = Some(EmbeddingFailure {
            message,
            at: Instant::now(),
            reported: repeated && failure.as_ref().is_some_and(|known| known.reported),
        });
    }
    fn clear_failure(&self) {
        if let Ok(mut failure) = self.failure.lock() {
            *failure = None;
        }
    }
    /// The recorded failure while its retry delay has not passed.
    fn cooling_down(&self) -> Option<String> {
        let failure = self.failure.lock().ok()?;
        failure
            .as_ref()
            .filter(|failure| failure.at.elapsed() < EMBEDDING_RETRY_AFTER)
            .map(|failure| failure.message.clone())
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
        self.clear_failure();
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
        self.clear_failure();
        Ok(())
    }
    pub fn disable_embeddings(&self) -> Result<()> {
        *self
            .embedder
            .lock()
            .map_err(|_| anyhow::anyhow!("Embedding lock poisoned"))? = None;
        self.clear_failure();
        Ok(())
    }
    /// Embeds `text` with the configured model. Failures are recorded once and
    /// suppress further attempts until the retry delay passes.
    fn vector(&self, text: &str) -> Result<Option<(String, Vec<u8>)>> {
        if let Some(message) = self.cooling_down() {
            anyhow::bail!(message);
        }
        let result = {
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
        };
        match &result {
            Ok(Some(_)) => self.clear_failure(),
            Ok(None) => {}
            Err(error) => self.record_failure(error),
        }
        result
    }
    /// Saves `record` with its content and source redacted of credentials
    /// and returns the record as stored. Saving the same record again is a
    /// no-op; reusing its id for different content is an error.
    pub fn remember_sync(&self, mut record: MemoryRecord) -> Result<MemoryRecord> {
        record.content = redaction::redact_secrets(&record.content);
        record.source = redaction::redact_secrets(&record.source);
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
            crate::memory_facts::reject_deleted(conn, existing.id)?;
            Ok((
                same_record(conn, &existing)?,
                crate::memory_facts::generation(conn, &existing.scope)?,
            ))
        })?;
        if exists {
            return Ok(record);
        }
        // Failures are recorded (and logged once) by `vector`; the memory is
        // still saved for lexical recall.
        let vector = self.vector(&record.content).unwrap_or(None);
        let stored = record.clone();
        self.store.call(move|conn|{let tx=conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            crate::memory_facts::reject_deleted(&tx,record.id)?;
            if same_record(&tx, &record)? { tx.commit()?; return Ok(()); }
            ensure!(crate::memory_facts::generation(&tx,&record.scope)?==generation,"Project facts changed during this save; refresh and retry");
            if let Some(previous)=record.supersedes {
                let previous_state:Option<(String,bool,String)>=tx.query_row("SELECT scope,active,json FROM memories WHERE id=?1",[previous.to_string()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
                let Some((scope,active,json))=previous_state else { anyhow::bail!("Correct the current memory in the same project") };
                ensure!(scope==record.scope&&active,"Correct the current memory in the same project");
                let replaced:MemoryRecord=serde_json::from_str(&json)?;
                ensure!(record.origin.tier()<=replaced.origin.tier(),"This fact was {}; replacing it needs a correction the user approves",replaced.origin.label());
                tx.execute("UPDATE memories SET active=0 WHERE id=?1",[previous.to_string()])?; delete_vectors(&tx, &previous.to_string())?;
            }
            // The vector lives only in its model's vec0 table; `embedding_model`
            // records which index holds it so reindexing can find stale rows.
            let mut model=None;
            if let Some((vector_model, vector_embedding)) = &vector {
                tx.execute_batch("SAVEPOINT optional_memory_vector")?;
                let indexed = (|| -> Result<()> { let table = ensure_vector_table(&tx, vector_model, vector_embedding.len()/4)?; tx.execute(&format!("INSERT INTO {table}(id,scope,embedding) VALUES(?1,?2,?3)"),params![record.id.to_string(),record.scope,vector_embedding])?; Ok(()) })();
                if indexed.is_ok() { model = Some(vector_model.clone()); } else { tx.execute_batch("ROLLBACK TO optional_memory_vector")?; tracing::warn!("Optional semantic index unavailable; saving lexical memory"); }
                tx.execute_batch("RELEASE optional_memory_vector")?;
            }
            tx.execute("INSERT INTO memories(id,scope,content,json,embedding_model) VALUES(?1,?2,?3,?4,?5)",params![record.id.to_string(),record.scope,record.content,serde_json::to_string(&record)?,model])?;
            tx.execute("INSERT INTO memories_fts(id,scope,content) VALUES(?1,?2,?3)",params![record.id.to_string(),record.scope,record.content])?;
            crate::memory_facts::inserted(&tx,&record)?;tx.commit()?;Ok(())})?;
        Ok(stored)
    }
    /// Like [`Self::recall_sync`], with the version each recalled fact
    /// corrected, so a reader sees what changed and since when.
    pub fn recall_with_history(
        &self,
        scope: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<RecalledFact>> {
        let records = self.recall_sync(scope, query, limit)?;
        self.store.call(move |conn| {
            let mut previous = conn.prepare("SELECT json FROM memories WHERE id=?1")?;
            records
                .into_iter()
                .map(|record| {
                    let replaced = match record.supersedes {
                        Some(id) => previous
                            .query_row([id.to_string()], |row| row.get::<_, String>(0))
                            .optional()?
                            .map(|json| serde_json::from_str::<MemoryRecord>(&json))
                            .transpose()?
                            .map(|old| PreviousVersion {
                                id: old.id,
                                content: old.content,
                                origin: old.origin,
                                created_at: old.created_at,
                                until: record.created_at,
                            }),
                        None => None,
                    };
                    Ok(RecalledFact {
                        record,
                        previously: replaced,
                    })
                })
                .collect()
        })
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
        let vector = self.vector(query).unwrap_or(None);
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
        let mut count = 0;
        for (id, content) in records {
            let (model, vector) = self
                .vector(&content)?
                .context("Embedding configuration was disabled during reindex")?;
            let indexed = self.store.call(move |conn| {
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let indexed = store_reindexed_vector(&tx, &id, &model, &vector)?;
                tx.commit()?;
                Ok(indexed)
            })?;
            count += usize::from(indexed);
        }
        Ok(count)
    }
}

/// Stores one reindexed vector. Embedding runs outside the transaction, so the
/// memory may have been forgotten or superseded meanwhile: such a memory is
/// skipped (false) rather than failing the whole reindex or giving a superseded
/// version a vector that semantic recall could return.
fn store_reindexed_vector(
    tx: &rusqlite::Connection,
    id: &str,
    model: &str,
    vector: &[u8],
) -> Result<bool> {
    let scope: Option<String> = tx
        .query_row(
            "SELECT scope FROM memories WHERE id=?1 AND active=1",
            [id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(scope) = scope else {
        return Ok(false);
    };
    delete_vectors(tx, id)?;
    let table = ensure_vector_table(tx, model, vector.len() / 4)?;
    tx.execute(
        &format!("INSERT INTO {table}(id,scope,embedding) VALUES(?1,?2,?3)"),
        params![id, scope, vector],
    )?;
    tx.execute(
        "UPDATE memories SET embedding_model=?1 WHERE id=?2",
        params![model, id],
    )?;
    Ok(true)
}
#[async_trait]
impl MemoryBackend for SqliteMemory {
    async fn remember(&self, record: MemoryRecord) -> Result<()> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.remember_sync(record).map(drop)).await?
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
    let hash = hex::encode(Sha256::digest(format!("{model}:{dimensions}")));
    let table = format!("memory_vec_{}", &hash[..24]);
    conn.execute_batch(&format!("CREATE VIRTUAL TABLE IF NOT EXISTS {table} USING vec0(id TEXT PRIMARY KEY,scope TEXT PARTITION KEY,embedding float[{dimensions}] distance_metric=cosine)"))?;
    conn.execute(
        "INSERT OR IGNORE INTO memory_vector_tables(name,model,dimensions) VALUES(?1,?2,?3)",
        params![table, model, dimensions as i64],
    )?;
    Ok(table)
}
pub(crate) fn delete_vectors(conn: &rusqlite::Connection, id: &str) -> Result<()> {
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
mod vector_tests;
