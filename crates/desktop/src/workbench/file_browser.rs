//! A directory page is the only file list retained by the native interface.
//! Requests are correlated and cancelled on navigation; the host owns discovery.
use super::*;
use std::sync::Arc;
use workbench_runtime::workspace::{FileCursor, FileEntry, FileOptions, FilePage};

const PAGE_SIZE: usize = 500;

#[derive(Default)]
pub(super) struct FileBrowserUi {
    pub entries: Arc<Vec<FileEntry>>,
    pub directory: Option<PathBuf>,
    pub next: Option<FileCursor>,
    pub after: Option<FileCursor>,
    pub file_count: usize,
    pub directory_count: usize,
    pub indexing: bool,
    pub watch_limited: bool,
    pub notice: String,
    revision: u64,
    loaded: bool,
    refresh_pending: bool,
    request: Option<FileRequest>,
}
struct FileRequest {
    id: Uuid,
    root: PathBuf,
    directory: PathBuf,
    after: Option<FileCursor>,
}
impl FileBrowserUi {
    #[cfg(test)]
    #[allow(dead_code)] // Used by the standalone native validation target.
    pub(super) fn validation_request(&self) -> Option<Uuid> {
        self.request.as_ref().map(|request| request.id)
    }
    pub fn loading(&self) -> bool {
        self.request.is_some()
    }
}

impl Workbench {
    pub(super) fn reset_file_browser(&mut self) {
        if let Some(request) = self.file_browser.request.take()
            && let Some(host) = &self.host
        {
            let _ = host.cancel_project_search(request.id);
        }
        self.file_browser = FileBrowserUi::default();
    }
    pub(super) fn browse_directory(&mut self, directory: PathBuf, cx: &mut Context<Self>) {
        let Some(root) = &self.workspace else {
            return;
        };
        if !directory.starts_with(root) {
            return;
        }
        self.file_browser.entries = Default::default();
        self.file_browser.loaded = false;
        self.file_browser.after = None;
        self.file_browser.next = None;
        self.file_browser.directory = Some(directory);
        self.load_file_page(None, cx);
    }
    pub(super) fn load_file_page(&mut self, after: Option<FileCursor>, cx: &mut Context<Self>) {
        let Some(root) = self.workspace.clone() else {
            return;
        };
        let directory = self
            .file_browser
            .directory
            .clone()
            .unwrap_or_else(|| root.clone());
        if !directory.starts_with(&root) || !self.workspace_ready {
            return;
        }
        if let Some(old) = self.file_browser.request.take()
            && let Some(host) = &self.host
        {
            let _ = host.cancel_project_search(old.id);
        }
        let id = Uuid::new_v4();
        let options = FileOptions {
            directory: directory.clone(),
            after: after.clone(),
            limit: PAGE_SIZE,
        };
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.workspace_files(id, root.clone(), options));
        if result.is_ok() {
            self.file_browser.directory = Some(directory.clone());
            self.file_browser.refresh_pending = false;
            self.file_browser.notice.clear();
            self.file_browser.request = Some(FileRequest {
                id,
                root,
                directory,
                after,
            });
        }
        self.result(result, cx);
    }
    pub(super) fn file_page_result(
        &mut self,
        id: Uuid,
        result: Result<FilePage, String>,
        cx: &mut Context<Self>,
    ) {
        if self
            .file_browser
            .request
            .as_ref()
            .is_none_or(|request| request.id != id)
        {
            return;
        }
        let Some(request) = self.file_browser.request.take() else {
            return;
        };
        if self.workspace.as_ref() != Some(&request.root)
            || self.file_browser.directory.as_ref() != Some(&request.directory)
        {
            return;
        }
        match result {
            Ok(page)
                if page.root == request.root
                    && page.directory == request.directory
                    && page.entries.len() <= PAGE_SIZE
                    && page
                        .entries
                        .iter()
                        .all(|entry| entry.path.parent() == Some(page.directory.as_path())) =>
            {
                // The catalogue can advance before its queued progress event.
                // A newer authoritative page is valid; only an older page needs
                // refreshing. Requiring equality could cause repeated reads.
                if page.revision < self.file_browser.revision {
                    self.load_file_page(None, cx);
                    return;
                }
                self.file_browser.revision = page.revision;
                self.file_browser.refresh_pending = false;
                self.file_browser.entries = Arc::new(page.entries);
                self.file_scroll = UniformListScrollHandle::new();
                self.file_browser.next = page.next;
                self.file_browser.after = request.after;
                // The first progress message can precede the root's shallow
                // scan. Wait for a later progress message to refresh an empty
                // root page, rather than hiding all folders until DFS finishes.
                self.file_browser.loaded = !(page.indexing
                    && page.directory == page.root
                    && self.file_browser.entries.is_empty());
                self.file_browser.file_count = page.file_count;
                self.file_browser.directory_count = page.directory_count;
                self.file_browser.indexing = page.indexing;
                self.file_browser.notice = if page.unreadable > 0 {
                    format!("{} items could not be read", page.unreadable)
                } else {
                    String::new()
                };
            }
            Ok(_) => self.file_browser.notice = "Project changed. Refresh this folder.".into(),
            Err(message) => {
                self.file_browser.notice = message;
                self.file_browser.next = None;
                if self.file_browser.refresh_pending {
                    self.load_file_page(None, cx);
                }
            }
        }
        cx.notify();
    }
    pub(super) fn update_file_browser(
        &mut self,
        index: &workbench_runtime::workspace::FileIndex,
        cx: &mut Context<Self>,
    ) {
        if index.revision < self.file_browser.revision {
            return;
        }
        let was_indexing = self.file_browser.indexing;
        self.file_browser.indexing = index.indexing;
        self.file_browser.watch_limited = !index.watching;
        self.file_browser.file_count = index.file_count;
        self.file_browser.directory_count = index.directory_count;
        self.file_browser.revision = index.revision;
        let directory = self.file_browser.directory.as_ref().unwrap_or(&index.root);
        let relevant = index
            .changed
            .iter()
            .chain(&index.removed)
            .any(|path| path == directory || path.parent() == Some(directory.as_path()));
        let invalidated = (was_indexing && !index.indexing)
            || (!index.indexing && (relevant || !index.changes_complete));
        if invalidated || (!self.file_browser.loaded && !self.file_browser.loading()) {
            if self.file_browser.loading() {
                self.file_browser.refresh_pending = true;
            } else {
                self.load_file_page(None, cx);
            }
        }
    }
}
