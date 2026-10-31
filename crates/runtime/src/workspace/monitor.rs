use super::{
    FileIndex, WorkspaceEvent,
    index::{self, Delta, Entry},
    rules::Visibility,
};
use anyhow::{Result, ensure};
use notify::{EventKind, RecursiveMode, Watcher};
use parking_lot::{Mutex, RwLock};
use rusqlite::Connection;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

const DIRTY_LIMIT: usize = 4096;
#[derive(Default)]
struct Pending {
    repair: bool,
    paths: BTreeMap<PathBuf, bool>,
}

/// Event storms allocate at most 4,096 path identities. Overflow changes the
/// operation to one streamed repair rather than dropping changes silently.
pub(super) struct DirtyPaths {
    pending: Mutex<Pending>,
    wake: async_channel::Sender<()>,
    incoming: async_channel::Receiver<()>,
    watch_failed: AtomicBool,
}
impl DirtyPaths {
    pub fn new() -> Self {
        let (wake, incoming) = async_channel::bounded(1);
        Self {
            pending: Mutex::new(Pending::default()),
            wake,
            incoming,
            watch_failed: AtomicBool::new(false),
        }
    }
    pub fn repair(&self) {
        let mut pending = self.pending.lock();
        pending.repair = true;
        pending.paths.clear();
        let _ = self.wake.try_send(());
    }
    fn path(&self, path: PathBuf, subtree: bool) {
        let mut pending = self.pending.lock();
        if pending.repair {
            return;
        }
        if pending.paths.len() >= DIRTY_LIMIT && !pending.paths.contains_key(&path) {
            pending.repair = true;
            pending.paths.clear();
        } else {
            pending
                .paths
                .entry(path)
                .and_modify(|prior| *prior |= subtree)
                .or_insert(subtree);
        }
        let _ = self.wake.try_send(());
    }
    fn take(&self) -> Pending {
        while self.incoming.try_recv().is_ok() {}
        std::mem::take(&mut *self.pending.lock())
    }
    fn observe(&self, event: notify::Result<notify::Event>, root: &Path, cache: &Path) {
        let Ok(event) = event else {
            self.watch_failed.store(true, Ordering::Release);
            self.repair();
            return;
        };
        if matches!(event.kind, EventKind::Access(_)) {
            return;
        }
        if event.need_rescan() {
            self.repair();
            return;
        }
        let subtree = matches!(
            event.kind,
            EventKind::Create(_)
                | EventKind::Remove(_)
                | EventKind::Modify(notify::event::ModifyKind::Name(_))
                | EventKind::Any
                | EventKind::Other
        );
        for path in event.paths {
            if !path.starts_with(root) || path.starts_with(cache) {
                continue;
            }
            if path
                .file_name()
                .is_some_and(|name| name == ".gitignore" || name == ".ignore" || name == ".git")
                || path.ends_with(".git/info/exclude")
            {
                self.repair();
                continue;
            }
            if path
                .strip_prefix(root)
                .is_ok_and(|path| path.components().any(|part| part.as_os_str() == ".git"))
            {
                continue;
            }
            // Coarse directory-only notifications require repair; precise file
            // events below update only the named file or created/renamed subtree.
            if path == root {
                if subtree {
                    self.repair();
                }
                continue;
            }
            self.path(path, subtree);
        }
    }
}
pub(super) fn walker(root: &Path, cache: &Path) -> ignore::WalkBuilder {
    let excluded = cache.to_owned();
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(false)
        // ignore0.4 only resolves gitfile/commondir when require_git is true.
        // Outside repositories we still honour explicit .gitignore files.
        .require_git(
            root.ancestors()
                .any(|directory| directory.join(".git").exists() || directory.join(".jj").exists()),
        )
        .follow_links(false)
        .max_depth(Some(1))
        .filter_entry(move |entry| {
            entry.file_name() != ".git" && !entry.path().starts_with(&excluded)
        });
    builder
}
struct Progress {
    root: PathBuf,
    latest: Arc<RwLock<Option<Arc<FileIndex>>>>,
    events: async_channel::Sender<WorkspaceEvent>,
    last: Instant,
}
impl Progress {
    fn publish(&mut self, conn: &Connection, delta: &Delta, force: bool) -> Result<()> {
        if force || self.last.elapsed() >= Duration::from_millis(500) {
            let snapshot = Arc::new(index::metadata(conn, &self.root, delta)?);
            *self.latest.write() = Some(snapshot.clone());
            let _ = self.events.try_send(WorkspaceEvent::Indexed(snapshot));
            self.last = Instant::now();
        }
        Ok(())
    }
}
struct Job {
    conn: Connection,
    root: PathBuf,
    cache: PathBuf,
    progress: Progress,
    stop: CancellationToken,
}
impl Job {
    fn seed(&mut self, path: &Path, delta: &mut Delta) -> Result<usize> {
        index::ScanContext {
            root: &self.root,
            cache: &self.cache,
            cancel: &self.stop,
            stop: &self.stop,
        }
        .seed(&mut self.conn, path, delta, |conn, delta| {
            self.progress.publish(conn, delta, false)
        })
    }
    fn tree(&mut self, path: &Path, whole_project: bool, delta: &mut Delta) -> Result<usize> {
        let current = index::epoch(&self.conn)?;
        let mut unreadable = self.seed(path, delta)?;
        self.progress.publish(&self.conn, delta, true)?;
        while let Some(directory) = index::next_directory(
            &self.conn,
            current,
            if whole_project { None } else { Some(path) },
        )? {
            ensure!(!self.stop.is_cancelled(), "Project refresh cancelled");
            unreadable += self.seed(&directory, delta)?;
        }
        Ok(unreadable)
    }
    fn full(&mut self) -> Result<()> {
        ensure!(!self.stop.is_cancelled(), "Project refresh cancelled");
        ensure!(
            self.root.is_dir(),
            "The selected project folder is no longer available"
        );
        let current = index::begin_scan(&self.conn)?;
        let mut delta = Delta {
            incomplete: true,
            ..Delta::default()
        };
        self.progress.publish(&self.conn, &delta, true)?;
        let unreadable = self.tree(&self.root.clone(), true, &mut delta)?;
        // An unreadable subtree is not proof its cached children were deleted.
        if unreadable == 0 {
            index::remove(&mut self.conn, None, Some(current), &mut delta, &self.stop)?;
        }
        ensure!(!self.stop.is_cancelled(), "Project refresh cancelled");
        ensure!(self.conn.execute("UPDATE meta SET indexing=0,revision=revision+1,unreadable=?1 WHERE id=1 AND epoch=?2",rusqlite::params![super::index::sql_int(unreadable),current])?==1,"The project changed; refresh files again");
        self.progress.publish(&self.conn, &delta, true)?;
        Ok(())
    }
    fn incremental(&mut self, changes: BTreeMap<PathBuf, bool>) -> Result<()> {
        let mut visibility = Visibility::new(&self.root);
        let mut delta = Delta::default();
        let subtree_scan = changes
            .iter()
            .any(|(path, subtree)| *subtree && path.is_dir());
        let current = if subtree_scan {
            let current = index::begin_scan(&self.conn)?;
            self.progress.publish(
                &self.conn,
                &Delta {
                    incomplete: true,
                    ..Delta::default()
                },
                true,
            )?;
            current
        } else {
            index::epoch(&self.conn)?
        };
        let mut unreadable = 0;
        for (path, subtree) in changes {
            ensure!(!self.stop.is_cancelled(), "Project refresh cancelled");
            let metadata = match std::fs::symlink_metadata(&path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    index::remove(&mut self.conn, Some(&path), None, &mut delta, &self.stop)?;
                    continue;
                }
                Err(_) => {
                    unreadable += 1;
                    continue;
                }
                Ok(metadata) => metadata,
            };
            if !visibility.visible(&path, metadata.is_dir()) {
                index::remove(&mut self.conn, Some(&path), None, &mut delta, &self.stop)?;
                continue;
            }
            if metadata.is_dir() && subtree {
                let errors = self.tree(&path, false, &mut delta)?;
                if errors == 0 {
                    index::prune_scope(&mut self.conn, &path, current, &mut delta, &self.stop)?;
                } else {
                    delta.incomplete = true;
                    unreadable += errors;
                }
            } else {
                // A directory can be removed and replaced by a file within one
                // native event burst. Its former descendants must not survive.
                let was_directory: bool = self.conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM entries WHERE path=?1 AND kind=1)",
                    [super::paths::encode(&path)],
                    |row| row.get(0),
                )?;
                if was_directory && !metadata.is_dir() {
                    index::remove(&mut self.conn, Some(&path), None, &mut delta, &self.stop)?;
                }
                match Entry::read(&path, &self.root) {
                    Ok(Some(entry)) => {
                        let before = delta.count;
                        index::upsert(&mut self.conn, &[entry], current, &mut delta, &self.stop)?;
                        // A named data event is evidence even on filesystems
                        // with coarse or deliberately preserved timestamps.
                        if !metadata.is_dir() && delta.count == before {
                            delta.changed(path.clone());
                        }
                    }
                    Ok(None) => {
                        index::remove(&mut self.conn, Some(&path), None, &mut delta, &self.stop)?
                    }
                    Err(_) => unreadable += 1,
                }
            }
        }
        if delta.any() || unreadable != 0 || subtree_scan {
            ensure!(!self.stop.is_cancelled(), "Project refresh cancelled");
            ensure!(self.conn.execute("UPDATE meta SET revision=revision+1,unreadable=?1,indexing=0 WHERE id=1 AND epoch=?2",rusqlite::params![super::index::sql_int(unreadable),current])?==1,"The project changed; refresh files again");
            self.progress.publish(&self.conn, &delta, true)?;
        }
        Ok(())
    }
}

/// Keeps a native project watcher alive on a thread of its own.
///
/// Registering and releasing a native watcher are blocking calls into the
/// operating system's file event service, which can take seconds while that
/// service is busy. They never run on the async runtime: a runtime worker held
/// by them stalls unrelated host work, and runtime shutdown would wait for
/// them. Dropping the handle releases the watcher in the background without
/// waiting for the release to finish.
struct NativeWatch {
    _release: std::sync::mpsc::Sender<()>,
}
impl NativeWatch {
    /// Runs `register` on a new thread. The receiver reports whether
    /// recursive native watching is active; it reports an error when the
    /// thread could not be started.
    fn start<W: 'static>(
        register: impl FnOnce() -> Option<W> + Send + 'static,
    ) -> (Self, tokio::sync::oneshot::Receiver<bool>) {
        let (release, released) = std::sync::mpsc::channel::<()>();
        let (ready, registered) = tokio::sync::oneshot::channel();
        let spawned = std::thread::Builder::new()
            .name("xylo-project-watch".into())
            .spawn(move || {
                let watcher = register();
                if ready.send(watcher.is_some()).is_ok() && watcher.is_some() {
                    // Returns once the handle is dropped.
                    let _ = released.recv();
                }
                drop(watcher);
            });
        if let Err(error) = spawned {
            tracing::warn!(%error, "Project change watching is unavailable");
        }
        (Self { _release: release }, registered)
    }
}

/// Watches `root` recursively and its parent non-recursively, so that the
/// project folder itself being renamed or removed is noticed. `None` means
/// recursive native watching is unavailable.
fn register(
    root: &Path,
    cache: PathBuf,
    dirty: Arc<DirtyPaths>,
) -> Option<notify::RecommendedWatcher> {
    let callback_root = root.to_owned();
    let mut watcher =
        notify::recommended_watcher(move |event| dirty.observe(event, &callback_root, &cache))
            .ok()?;
    watcher.watch(root, RecursiveMode::Recursive).ok()?;
    if let Some(parent) = root.parent() {
        let _ = watcher.watch(parent, RecursiveMode::NonRecursive);
    }
    Some(watcher)
}

/// How long the first scan waits for native watcher registration before it
/// starts without it. Registration normally takes milliseconds.
const REGISTRATION_GRACE: Duration = Duration::from_millis(250);

/// Waits for a pending watcher registration and reports whether recursive
/// native watching is now active. Never completes once the result is known.
async fn registration_result(
    registration: &mut Option<tokio::sync::oneshot::Receiver<bool>>,
) -> bool {
    let Some(receiver) = registration.as_mut() else {
        return std::future::pending().await;
    };
    let registered = receiver.await.unwrap_or(false);
    *registration = None;
    registered
}

pub(super) async fn run(
    root: PathBuf,
    cache: PathBuf,
    database: PathBuf,
    latest: Arc<RwLock<Option<Arc<FileIndex>>>>,
    dirty: Arc<DirtyPaths>,
    events: async_channel::Sender<WorkspaceEvent>,
    stop: CancellationToken,
) {
    let result = observe(
        root.clone(),
        cache,
        database,
        latest,
        dirty,
        events.clone(),
        stop.clone(),
        register,
    )
    .await;
    if !stop.is_cancelled()
        && let Err(error) = result
    {
        tokio::select! {
            _=stop.cancelled()=>{},
            _=events.send(WorkspaceEvent::Unavailable{root,message:error.to_string()})=>{},
        }
    }
}
#[allow(clippy::too_many_arguments)]
async fn observe<W: 'static>(
    root: PathBuf,
    cache: PathBuf,
    database: PathBuf,
    latest: Arc<RwLock<Option<Arc<FileIndex>>>>,
    dirty: Arc<DirtyPaths>,
    events: async_channel::Sender<WorkspaceEvent>,
    stop: CancellationToken,
    register: impl FnOnce(&Path, PathBuf, Arc<DirtyPaths>) -> Option<W> + Send + 'static,
) -> Result<()> {
    tokio::fs::create_dir_all(&cache).await?;
    let cache = tokio::fs::canonicalize(cache).await?;
    let (watch, registered) = NativeWatch::start({
        let root = root.clone();
        let cache = cache.clone();
        let dirty = dirty.clone();
        move || register(&root, cache, dirty)
    });
    let mut watch = Some(watch);
    // A watcher that is live before the first scan starts misses nothing.
    // When registration is slow, the scan starts anyway so the catalogue is
    // not held hostage by the file event service; a late watcher then
    // triggers one repair that covers changes made before it was live.
    let mut registration = Some(registered);
    let mut native = tokio::select! {
        _ = stop.cancelled() => return Ok(()),
        registered = registration_result(&mut registration) => registered,
        _ = tokio::time::sleep(REGISTRATION_GRACE) => false,
    };
    if registration.is_none() && !native {
        watch = None;
    }
    let initialize_root = root.clone();
    let conn = tokio::task::spawn_blocking(move || index::open_writer(&database, &initialize_root))
        .await??;
    conn.execute("UPDATE meta SET watching=?1 WHERE id=1", [native])?;
    let mut job = Job {
        conn,
        root: root.clone(),
        cache,
        progress: Progress {
            root: root.clone(),
            latest: latest.clone(),
            events: events.clone(),
            last: Instant::now(),
        },
        stop: stop.clone(),
    };
    let mut initial = true;
    loop {
        if stop.is_cancelled() {
            break;
        }
        let pending = dirty.take();
        let repair = initial || pending.repair;
        initial = false;
        let (returned, result) = tokio::task::spawn_blocking(move || {
            let result = if repair {
                job.full()
            } else {
                job.incremental(pending.paths)
            };
            (job, result)
        })
        .await?;
        job = returned;
        if stop.is_cancelled() {
            break;
        }
        let had_error = result.is_err();
        if let Err(error) = result {
            tokio::select! {
                _=stop.cancelled()=>break,
                _=events.send(WorkspaceEvent::Unavailable{root:root.clone(),message:error.to_string()})=>{},
            }
        }
        if native && dirty.watch_failed.load(Ordering::Acquire) {
            native = false;
            watch = None;
            job.conn
                .execute("UPDATE meta SET watching=0 WHERE id=1", [])?;
            job.progress.publish(
                &job.conn,
                &Delta {
                    incomplete: true,
                    ..Delta::default()
                },
                true,
            )?;
        }
        // Intermediate progress may be coalesced. Always deliver the final state
        // while allowing a project switch to interrupt a full event queue.
        let snapshot = latest.read().clone();
        if let Some(snapshot) = snapshot {
            tokio::select! {
                _=stop.cancelled()=>break,
                _=events.send(WorkspaceEvent::Indexed(snapshot))=>{},
            }
        }
        if native && !had_error {
            tokio::select! {
                _=stop.cancelled()=>break,
                _=dirty.incoming.recv()=>{},
            }
        } else {
            // Failed recursive watching never retries in a tight loop. The
            // catalogue stays usable and explicit refresh remains immediate.
            tokio::select! {
                _=stop.cancelled()=>break,
                _=dirty.incoming.recv()=>{},
                _=tokio::time::sleep(Duration::from_secs(if had_error{60}else{300}))=>dirty.repair(),
                registered=registration_result(&mut registration)=>{
                    if registered {
                        native = true;
                        job.conn.execute("UPDATE meta SET watching=1 WHERE id=1", [])?;
                        dirty.repair();
                    } else {
                        watch = None;
                    }
                },
            }
        }
        tokio::select! {
            _=stop.cancelled()=>break,
            _=tokio::time::sleep(Duration::from_millis(150))=>{},
        }
    }
    drop(watch);
    Ok(())
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod review_tests;
