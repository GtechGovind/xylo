//! Lazy expandable project tree. The host performs discovery; the UI retains
//! bounded directory windows and ignores every uncorrelated or obsolete reply.
use super::*;
use std::{collections::VecDeque, sync::Arc};
use xylo_runtime::workspace::{FileCursor, FileEntry, FileOptions, FilePage};
#[path = "file_tree.rs"]
mod tree;
use tree::{FileTree, MAX_DIRECTORIES, PAGE_SIZE};
pub(super) use tree::{RowContent, RowKey, TreeRow};

const MAX_PENDING: usize = 4;

#[derive(Default)]
pub(super) struct FileBrowserUi {
    /// Visible entries only, retained for file-change/search integration.
    pub entries: Arc<Vec<FileEntry>>,
    pub tree: FileTree,
    pub focus: Option<FocusHandle>,
    pub file_count: usize,
    pub directory_count: usize,
    pub indexing: bool,
    pub watch_limited: bool,
    pub notice: String,
    revision: u64,
    index_revision: u64,
    requests: BTreeMap<Uuid, FileRequest>,
    queue: VecDeque<PageRequest>,
}
struct FileRequest {
    root: PathBuf,
    directory: PathBuf,
    after: Option<FileCursor>,
}
struct PageRequest {
    directory: PathBuf,
    after: Option<FileCursor>,
}
impl FileBrowserUi {
    #[cfg(test)]
    #[allow(dead_code)]
    pub(super) fn validation_request(&self) -> Option<Uuid> {
        self.requests.keys().next().copied()
    }
    #[cfg(test)]
    #[allow(dead_code)]
    pub(super) fn validation_request_for(&self, path: &Path) -> Option<Uuid> {
        self.requests
            .iter()
            .find(|(_, request)| request.directory == path)
            .map(|(id, _)| *id)
    }
    // The interaction tests and separate native rendering harness wait for the
    // entire request queue; product rows display their own loading state.
    #[allow(dead_code)]
    pub fn loading(&self) -> bool {
        !self.requests.is_empty() || !self.queue.is_empty()
    }
    fn rebuild(&mut self) {
        self.tree.rebuild();
        self.entries = Arc::new(
            self.tree
                .rows
                .iter()
                .filter_map(|row| match &row.content {
                    RowContent::Entry(entry) if Some(&entry.path) != self.tree.root.as_ref() => {
                        Some(entry.clone())
                    }
                    _ => None,
                })
                .collect(),
        );
    }
}

impl Xylo {
    pub(super) fn reset_file_browser(&mut self) {
        if let Some(host) = &self.host {
            for id in self.file_browser.requests.keys() {
                let _ = host.cancel_project_search(*id);
            }
        }
        self.file_browser = FileBrowserUi::default();
    }

    /// Existing project-open/refresh entry point. Directory continuations are
    /// now owned by individual inline tree rows rather than a global page bar.
    pub(super) fn load_file_page(&mut self, after: Option<FileCursor>, cx: &mut Context<Self>) {
        let Some(root) = self.workspace.clone() else {
            return;
        };
        if !self.workspace_ready {
            return;
        }
        self.file_browser.tree.set_root(root.clone());
        self.file_browser
            .focus
            .get_or_insert_with(|| cx.focus_handle());
        self.queue_tree_page(root, after, true);
        self.pump_tree_requests(cx);
        self.file_browser.rebuild();
        cx.notify();
    }

    fn queue_tree_page(
        &mut self,
        directory: PathBuf,
        after: Option<FileCursor>,
        replace_pending: bool,
    ) {
        if !self.file_browser.tree.visible(&directory) {
            return;
        }
        if self
            .file_browser
            .requests
            .values()
            .any(|request| request.directory == directory)
        {
            if replace_pending
                && let Some(node) = self.file_browser.tree.directories.get_mut(&directory)
            {
                node.stale = true;
            }
            return;
        }
        self.file_browser
            .queue
            .retain(|request| request.directory != directory);
        if self.file_browser.queue.len() >= MAX_DIRECTORIES {
            return;
        }
        if let Some(node) = self.file_browser.tree.directories.get_mut(&directory) {
            node.loading = true;
            node.stale = false;
            node.notice = None;
        }
        self.file_browser
            .queue
            .push_back(PageRequest { directory, after });
    }

    fn pump_tree_requests(&mut self, cx: &mut Context<Self>) {
        while self.file_browser.requests.len() < MAX_PENDING {
            let Some(request) = self.file_browser.queue.pop_front() else {
                break;
            };
            let Some(root) = self.workspace.clone() else {
                break;
            };
            if !self.workspace_ready || !self.file_browser.tree.visible(&request.directory) {
                continue;
            }
            let id = Uuid::new_v4();
            let options = FileOptions {
                directory: request.directory.clone(),
                after: request.after.clone(),
                limit: PAGE_SIZE,
            };
            let result = self
                .host
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
                .and_then(|host| host.workspace_files(id, root.clone(), options));
            match result {
                Ok(()) => {
                    self.file_browser.requests.insert(
                        id,
                        FileRequest {
                            root,
                            directory: request.directory,
                            after: request.after,
                        },
                    );
                }
                Err(error) => {
                    if let Some(node) = self
                        .file_browser
                        .tree
                        .directories
                        .get_mut(&request.directory)
                    {
                        node.loading = false;
                        node.notice = Some(format!(
                            "Couldn't load this folder: {}",
                            bounded_tree_message(&error.to_string())
                        ));
                    }
                }
            }
        }
        self.file_browser.rebuild();
        cx.notify();
    }

    pub(super) fn file_page_result(
        &mut self,
        id: Uuid,
        result: Result<FilePage, String>,
        cx: &mut Context<Self>,
    ) {
        let Some(request) = self.file_browser.requests.remove(&id) else {
            return;
        };
        if self.workspace.as_ref() != Some(&request.root)
            || !self.file_browser.tree.visible(&request.directory)
        {
            self.pump_tree_requests(cx);
            return;
        }
        let stale = self
            .file_browser
            .tree
            .directories
            .get_mut(&request.directory)
            .map(|node| {
                node.loading = false;
                node.stale
            })
            .unwrap_or(false);
        match result {
            Ok(page) if valid_tree_page(&page, &request) => {
                let required = self
                    .file_browser
                    .tree
                    .directories
                    .get(&request.directory)
                    .map_or(0, |node| node.required_revision);
                if stale || page.revision < required {
                    self.queue_tree_page(request.directory, None, false);
                } else {
                    // A page for an unchanged branch can predate another
                    // branch's newer snapshot; do not regress project metadata.
                    if page.revision >= self.file_browser.revision {
                        self.file_browser.revision = page.revision;
                        self.file_browser.file_count = page.file_count;
                        self.file_browser.directory_count = page.directory_count;
                        self.file_browser.indexing = page.indexing;
                        self.file_browser.notice = if page.unreadable > 0 {
                            format!("{} items could not be read", page.unreadable)
                        } else {
                            String::new()
                        };
                    }
                    let empty_initial_scan = page.indexing && page.entries.is_empty();
                    self.file_browser.tree.apply_page(
                        &request.directory,
                        page.entries,
                        page.next,
                        page.revision,
                        request.after.is_some(),
                    );
                    if empty_initial_scan
                        && let Some(node) = self
                            .file_browser
                            .tree
                            .directories
                            .get_mut(&request.directory)
                    {
                        // Await another discovery event, never spin on an empty
                        // in-progress catalogue before its shallow seed commits.
                        node.loaded = false;
                    }
                    self.cancel_hidden_tree_requests();
                    self.queue_restored_tree_children();
                }
            }
            Ok(_) => {
                if let Some(node) = self
                    .file_browser
                    .tree
                    .directories
                    .get_mut(&request.directory)
                {
                    node.notice =
                        Some("Couldn't load this folder. Refresh files to try again.".into());
                }
            }
            Err(message) => {
                if stale || (request.after.is_some() && message.contains("project changed")) {
                    self.queue_tree_page(request.directory, None, false);
                } else if let Some(node) = self
                    .file_browser
                    .tree
                    .directories
                    .get_mut(&request.directory)
                {
                    node.notice = Some(bounded_tree_message(&message));
                }
            }
        }
        self.pump_tree_requests(cx);
    }

    fn queue_restored_tree_children(&mut self) {
        // Missing cached nodes can occur after a collapsed branch was evicted.
        // Existing evicted windows have an explicit Load folder row instead.
        let missing: Vec<_> = self
            .file_browser
            .tree
            .visible_directories()
            .into_iter()
            .filter(|path| !self.file_browser.tree.directories.contains_key(path))
            .collect();
        for path in missing {
            if self.file_browser.tree.expand(&path) {
                self.queue_tree_page(path, None, false);
            }
        }
    }

    fn cancel_hidden_tree_requests(&mut self) {
        let hidden: Vec<_> = self
            .file_browser
            .requests
            .iter()
            .filter(|(_, request)| !self.file_browser.tree.visible(&request.directory))
            .map(|(id, request)| (*id, request.directory.clone()))
            .collect();
        for (id, path) in hidden {
            self.file_browser.requests.remove(&id);
            if let Some(host) = &self.host {
                let _ = host.cancel_project_search(id);
            }
            if let Some(node) = self.file_browser.tree.directories.get_mut(&path) {
                node.loading = false;
            }
        }
        self.file_browser
            .queue
            .retain(|request| self.file_browser.tree.visible(&request.directory));
        let live: BTreeSet<_> = self
            .file_browser
            .requests
            .values()
            .map(|request| request.directory.clone())
            .chain(
                self.file_browser
                    .queue
                    .iter()
                    .map(|request| request.directory.clone()),
            )
            .collect();
        for (path, node) in &mut self.file_browser.tree.directories {
            if !live.contains(path) {
                node.loading = false;
            }
        }
    }

    pub(super) fn toggle_tree_folder(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.file_browser.tree.expanded.contains(&path) {
            self.file_browser.tree.collapse(&path);
            self.cancel_hidden_tree_requests();
        } else if self.file_browser.tree.expand(&path) {
            self.file_browser.notice.clear();
            let load = self
                .file_browser
                .tree
                .directories
                .get(&path)
                .is_none_or(|node| {
                    !node.loaded || node.stale || node.revision < node.required_revision
                });
            if load {
                self.queue_tree_page(path.clone(), None, false);
            }
            let restore = self
                .file_browser
                .tree
                .visible_directories()
                .into_iter()
                .filter(|child| {
                    child != &path
                        && self
                            .file_browser
                            .tree
                            .directories
                            .get(child)
                            .is_some_and(|node| {
                                !node.loading
                                    && !node.deferred
                                    && (!node.loaded
                                        || node.stale
                                        || node.revision < node.required_revision)
                            })
                })
                .collect::<Vec<_>>();
            for child in restore {
                self.queue_tree_page(child, None, false);
            }
            self.queue_restored_tree_children();
        } else {
            self.file_browser.notice =
                "Collapse another folder before opening more, or open this folder as a project."
                    .into();
        }
        self.pump_tree_requests(cx);
    }

    pub(super) fn activate_tree_row(
        &mut self,
        key: RowKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(row) = self
            .file_browser
            .tree
            .rows
            .iter()
            .find(|row| row.key.as_ref() == Some(&key))
            .cloned()
        else {
            return;
        };
        self.file_browser.tree.selected = Some(key);
        if let Some(focus) = &self.file_browser.focus {
            window.focus(focus, cx);
        }
        match row.content {
            RowContent::Entry(entry) if entry.is_dir => self.toggle_tree_folder(entry.path, cx),
            RowContent::Entry(entry) => {
                if self.editor_dirty {
                    self.error =
                        Some("Save the current editor changes before opening another file.".into());
                } else if entry
                    .path
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .is_some_and(|extension| {
                        matches!(extension.to_ascii_lowercase().as_str(), "docx" | "xlsx")
                    })
                {
                    self.document_open(entry.path, window, cx);
                } else {
                    self.request_editor_file(entry.path, None, false, cx);
                }
            }
            RowContent::More => {
                let next = self
                    .file_browser
                    .tree
                    .directories
                    .get(&row.path)
                    .and_then(|node| node.next.clone());
                if let Some(next) = next {
                    self.queue_tree_page(row.path, Some(next), false);
                }
                self.pump_tree_requests(cx);
            }
            RowContent::Earlier | RowContent::Load => {
                self.queue_tree_page(row.path, None, false);
                self.pump_tree_requests(cx);
            }
            RowContent::Loading | RowContent::Notice(_) => {}
        }
        cx.notify();
    }

    pub(super) fn tree_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.modifiers.control
            || event.keystroke.modifiers.platform
            || event.keystroke.modifiers.alt
        {
            return;
        }
        match event.keystroke.key.as_str() {
            "up" | "down" => {
                self.file_browser
                    .tree
                    .select_delta(if event.keystroke.key == "up" { -1 } else { 1 });
            }
            "left" => {
                if let Some(RowKey::Entry(path)) = self.file_browser.tree.selected.clone() {
                    if self.file_browser.tree.expanded.contains(&path) {
                        self.toggle_tree_folder(path, cx);
                    } else if let Some(parent) = path.parent()
                        && self.file_browser.tree.rows.iter().any(|row| {
                            row.key.as_ref() == Some(&RowKey::Entry(parent.to_path_buf()))
                        })
                    {
                        self.file_browser.tree.selected = Some(RowKey::Entry(parent.to_path_buf()));
                    }
                }
            }
            "right" => {
                if let Some(RowKey::Entry(path)) = self.file_browser.tree.selected.clone() {
                    let directory = self.file_browser.tree.rows.iter().any(|row| {
                        row.path == path
                            && matches!(&row.content, RowContent::Entry(entry) if entry.is_dir)
                    });
                    if directory && !self.file_browser.tree.expanded.contains(&path) {
                        self.toggle_tree_folder(path, cx);
                    } else if directory {
                        self.file_browser.tree.select_child();
                    }
                }
            }
            "enter" => {
                if let Some(key) = self.file_browser.tree.selected.clone() {
                    self.activate_tree_row(key, window, cx);
                }
            }
            _ => return,
        }
        if let Some(index) = self.file_browser.tree.selected_index() {
            self.file_scroll
                .scroll_to_item(index, ScrollStrategy::Nearest);
        }
        cx.stop_propagation();
        window.prevent_default();
        cx.notify();
    }

    pub(super) fn update_file_browser(
        &mut self,
        index: &xylo_runtime::workspace::FileIndex,
        cx: &mut Context<Self>,
    ) {
        if self.workspace.as_ref() != Some(&index.root)
            || index.revision < self.file_browser.index_revision
        {
            return;
        }
        let finished = self.file_browser.indexing && !index.indexing;
        self.file_browser.index_revision = index.revision;
        self.file_browser.watch_limited = !index.watching;
        if index.revision >= self.file_browser.revision {
            self.file_browser.indexing = index.indexing;
            self.file_browser.file_count = index.file_count;
            self.file_browser.directory_count = index.directory_count;
            self.file_browser.revision = index.revision;
        }
        self.file_browser.tree.set_root(index.root.clone());
        let visible: BTreeSet<_> = self
            .file_browser
            .tree
            .visible_directories()
            .into_iter()
            .collect();
        let mut refresh = Vec::new();
        for (path, node) in &mut self.file_browser.tree.directories {
            if node.loaded && node.revision >= index.revision {
                continue;
            }
            let relevant = index
                .changed
                .iter()
                .chain(&index.removed)
                .any(|changed| changed == path || changed.parent() == Some(path.as_path()));
            if finished || (!index.indexing && (relevant || !index.changes_complete)) {
                node.required_revision = index.revision;
                node.stale = true;
                if visible.contains(path) {
                    refresh.push(path.clone());
                }
            } else if visible.contains(path)
                && !node.loaded
                && !node.loading
                && !node.deferred
                && node.entries.is_empty()
            {
                refresh.push(path.clone());
            }
        }
        for path in refresh {
            self.queue_tree_page(path, None, true);
        }
        self.pump_tree_requests(cx);
    }
}

fn bounded_tree_message(message: &str) -> String {
    message
        .chars()
        .filter(|c| !c.is_control())
        .take(240)
        .collect()
}

fn valid_tree_page(page: &FilePage, request: &FileRequest) -> bool {
    let mut paths = BTreeSet::new();
    page.root == request.root
        && page.directory == request.directory
        && page.entries.len() <= PAGE_SIZE
        && request.after.as_ref().is_none_or(|cursor| {
            cursor.revision == page.revision
                && !page.entries.iter().any(|entry| entry.path == cursor.path)
        })
        && page.entries.iter().all(|entry| {
            entry.path.parent() == Some(page.directory.as_path()) && paths.insert(&entry.path)
        })
        && page.next.as_ref().is_none_or(|next| {
            next.revision == page.revision
                && page
                    .entries
                    .last()
                    .is_some_and(|entry| entry.path == next.path)
        })
}
