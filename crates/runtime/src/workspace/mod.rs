//! Rebuildable project catalogue and native change observation. File identities
//! live on disk; neither the host nor the UI retains a project-sized path list.
mod index;
mod monitor;
mod paths;
mod reader;
mod rules;
mod search;
pub use index::{FileCursor, FileEntry, FileOptions, FilePage};
pub use search::{SearchCursor, SearchHit, SearchOptions, SearchPage};

use anyhow::{Context, Result, ensure};
use parking_lot::{Mutex, RwLock};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio_util::sync::CancellationToken;

/// Small invalidation/progress message. A false `changes_complete` requires the
/// consumer to recheck its open file; the bounded delta is never a full index.
#[derive(Clone, Debug)]
pub struct FileIndex {
    pub root: PathBuf,
    pub revision: u64,
    pub file_count: usize,
    pub directory_count: usize,
    pub changed: Vec<PathBuf>,
    pub removed: Vec<PathBuf>,
    pub unreadable: usize,
    pub indexing: bool,
    pub changes_complete: bool,
    /// False means automatic updates use a slower background repair. Browsing,
    /// search and explicit refresh remain available.
    pub watching: bool,
}
#[derive(Clone, Debug)]
pub enum WorkspaceEvent {
    Indexed(Arc<FileIndex>),
    Unavailable { root: PathBuf, message: String },
}
struct Active {
    root: PathBuf,
    stop: CancellationToken,
    index: Arc<RwLock<Option<Arc<FileIndex>>>>,
    database: PathBuf,
    dirty: Arc<monitor::DirtyPaths>,
    queries: Arc<tokio::sync::Semaphore>,
}
struct Shared {
    active: Mutex<Option<Active>>,
    events: async_channel::Sender<WorkspaceEvent>,
    cache_root: PathBuf,
    stop: CancellationToken,
}
impl Drop for Shared {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}
#[derive(Clone)]
pub struct WorkspaceService(Arc<Shared>);
impl WorkspaceService {
    /// Isolated cache for callers without application paths (primarily tests).
    /// Production uses `with_cache` to reuse the private on-disk catalogue.
    pub fn new(events: async_channel::Sender<WorkspaceEvent>) -> Self {
        Self::with_cache(
            events,
            std::env::temp_dir().join(format!("workbench-workspace-{}", uuid::Uuid::new_v4())),
        )
    }
    pub fn with_cache(events: async_channel::Sender<WorkspaceEvent>, cache_root: PathBuf) -> Self {
        Self(Arc::new(Shared {
            active: Mutex::new(None),
            events,
            cache_root,
            stop: CancellationToken::new(),
        }))
    }
    /// Returns after selecting the root. Traversal and SQLite writes run on one
    /// cancellable blocking task, independent of the foreground Store executor.
    pub async fn open(&self, root: &Path) -> Result<()> {
        let root = tokio::fs::canonicalize(root).await?;
        ensure!(
            tokio::fs::metadata(&root).await?.is_dir(),
            "Choose a project folder"
        );
        let mut active = self.0.active.lock();
        if active
            .as_ref()
            .is_some_and(|old| old.root == root && !old.stop.is_cancelled())
        {
            return Ok(());
        }
        if let Some(old) = active.take() {
            old.stop.cancel();
        }
        let stop = self.0.stop.child_token();
        let index = Arc::new(RwLock::new(None));
        let dirty = Arc::new(monitor::DirtyPaths::new());
        let database = self.0.cache_root.join(paths::database_name(&root));
        tokio::spawn(monitor::run(
            root.clone(),
            self.0.cache_root.clone(),
            database.clone(),
            index.clone(),
            dirty.clone(),
            self.0.events.clone(),
            stop.clone(),
        ));
        *active = Some(Active {
            root,
            stop,
            index,
            database,
            dirty,
            queries: Arc::new(tokio::sync::Semaphore::new(2)),
        });
        Ok(())
    }
    pub fn refresh(&self) {
        if let Some(active) = self.0.active.lock().as_ref() {
            active.dirty.repair();
        }
    }
    pub fn snapshot(&self, root: &Path) -> Result<Arc<FileIndex>> {
        let active = self.0.active.lock();
        let active = active.as_ref().context("Open a project first")?;
        ensure!(active.root == root, "The project changed; try again");
        active
            .index
            .read()
            .clone()
            .context("Project files are still loading")
    }
    fn query_context(
        &self,
        root: &Path,
    ) -> Result<(PathBuf, CancellationToken, Arc<tokio::sync::Semaphore>)> {
        let active = self.0.active.lock();
        let active = active.as_ref().context("Open a project first")?;
        ensure!(
            active.root == root && !active.stop.is_cancelled(),
            "The project changed; try again"
        );
        Ok((
            active.database.clone(),
            active.stop.clone(),
            active.queries.clone(),
        ))
    }
    pub async fn files(
        &self,
        root: PathBuf,
        options: FileOptions,
        cancel: CancellationToken,
    ) -> Result<FilePage> {
        let root = tokio::fs::canonicalize(root).await?;
        let (database, project_stop, gate) = self.query_context(&root)?;
        let permit = tokio::select! { biased; _ = cancel.cancelled() => anyhow::bail!("File listing cancelled"), _ = project_stop.cancelled() => anyhow::bail!("The project changed; try again"), permit = gate.acquire_owned() => permit? };
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            index::files(&database, &root, options, &cancel, &project_stop)
        })
        .await?
    }
    pub async fn search(
        &self,
        root: PathBuf,
        options: SearchOptions,
        cancel: CancellationToken,
    ) -> Result<SearchPage> {
        let root = tokio::fs::canonicalize(root).await?;
        let (database, project_stop, gate) = self.query_context(&root)?;
        let permit = tokio::select! { biased; _ = cancel.cancelled() => anyhow::bail!("Search cancelled"), _ = project_stop.cancelled() => anyhow::bail!("The project changed; search again"), permit = gate.acquire_owned() => permit? };
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            search::search(&database, &root, options, &cancel, &project_stop)
        })
        .await?
    }
    pub fn shutdown(&self) {
        self.0.stop.cancel();
        if let Some(active) = self.0.active.lock().take() {
            active.stop.cancel();
        }
    }
}

#[cfg(test)]
mod tests;
