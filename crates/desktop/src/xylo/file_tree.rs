//! Bounded, purely in-memory presentation of lazy directory pages. This module
//! never reads the filesystem. The host remains the authority for every entry.
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
};
use xylo_runtime::workspace::{FileCursor, FileEntry};

pub(super) const PAGE_SIZE: usize = 500;
pub(super) const MAX_DIRECTORIES: usize = 128;
pub(super) const MAX_ENTRIES: usize = 32_000;
const WINDOW_ENTRIES: usize = 1_000;
const MAX_DEPTH: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::xylo) enum RowKey {
    Entry(PathBuf),
    More(PathBuf),
    Earlier(PathBuf),
    Load(PathBuf),
}

#[derive(Clone)]
pub(in crate::xylo) enum RowContent {
    Entry(FileEntry),
    More,
    Earlier,
    Load,
    Loading,
    Notice(String),
}

#[derive(Clone)]
pub(in crate::xylo) struct TreeRow {
    pub path: PathBuf,
    pub depth: usize,
    pub expanded: bool,
    pub key: Option<RowKey>,
    pub content: RowContent,
}

#[derive(Default)]
pub(in crate::xylo) struct Directory {
    pub entries: Vec<FileEntry>,
    pub next: Option<FileCursor>,
    pub windowed: bool,
    pub loaded: bool,
    pub loading: bool,
    pub stale: bool,
    pub deferred: bool,
    pub required_revision: u64,
    pub revision: u64,
    pub notice: Option<String>,
    touched: u64,
}

#[derive(Default)]
pub(in crate::xylo) struct FileTree {
    pub root: Option<PathBuf>,
    pub directories: BTreeMap<PathBuf, Directory>,
    pub expanded: BTreeSet<PathBuf>,
    pub rows: Arc<Vec<TreeRow>>,
    pub selected: Option<RowKey>,
    clock: u64,
}

impl FileTree {
    pub fn set_root(&mut self, root: PathBuf) {
        if self.root.as_ref() == Some(&root) {
            return;
        }
        *self = Self::default();
        self.expanded.insert(root.clone());
        self.directories.insert(root.clone(), Directory::default());
        self.selected = Some(RowKey::Entry(root.clone()));
        self.root = Some(root);
        self.rebuild();
    }

    pub fn visible_directories(&self) -> Vec<PathBuf> {
        self.rows
            .iter()
            .filter_map(|row| match &row.content {
                RowContent::Entry(entry) if entry.is_dir && row.expanded => Some(row.path.clone()),
                _ => None,
            })
            .collect()
    }

    pub fn visible(&self, directory: &Path) -> bool {
        self.rows.iter().any(|row| {
            row.path == directory && row.expanded && matches!(row.content, RowContent::Entry(_))
        })
    }

    pub fn selected_index(&self) -> Option<usize> {
        self.rows
            .iter()
            .position(|row| row.key.as_ref() == self.selected.as_ref() && row.key.is_some())
    }

    pub fn select_delta(&mut self, delta: isize) -> Option<usize> {
        let start = self.selected_index().unwrap_or(0) as isize;
        let mut index = start + delta;
        while index >= 0 && index < self.rows.len() as isize {
            let row = &self.rows[index as usize];
            if let Some(key) = &row.key {
                self.selected = Some(key.clone());
                return Some(index as usize);
            }
            index += delta.signum();
        }
        self.selected_index()
    }

    pub fn select_child(&mut self) {
        let Some(index) = self.selected_index() else {
            return;
        };
        let depth = self.rows[index].depth;
        if let Some(key) = self.rows[index + 1..]
            .iter()
            .take_while(|row| row.depth > depth)
            .find_map(|row| row.key.clone())
        {
            self.selected = Some(key);
        }
    }

    pub fn expand(&mut self, path: &Path) -> bool {
        if self
            .root
            .as_ref()
            .is_none_or(|root| !path.starts_with(root))
        {
            return false;
        }
        if self
            .root
            .as_ref()
            .and_then(|root| path.strip_prefix(root).ok())
            .is_some_and(|relative| relative.components().count() >= MAX_DEPTH)
        {
            return false;
        }
        self.evict_collapsed();
        if !self.directories.contains_key(path) && self.directories.len() >= MAX_DIRECTORIES {
            return false;
        }
        self.clock += 1;
        self.directories
            .entry(path.to_path_buf())
            .or_default()
            .touched = self.clock;
        self.expanded.insert(path.to_path_buf());
        self.rebuild();
        true
    }

    pub fn collapse(&mut self, path: &Path) {
        self.expanded.remove(path);
        // Child expansion choices remain cached, but hidden reads are cancelled
        // by the controller. Reopening the parent can restore them lazily.
        self.rebuild();
    }

    pub fn apply_page(
        &mut self,
        path: &Path,
        entries: Vec<FileEntry>,
        next: Option<FileCursor>,
        revision: u64,
        append: bool,
    ) -> bool {
        let incoming = entries.len();
        let previous = self
            .directories
            .get(path)
            .map_or(0, |node| node.entries.len());
        let roll = append && previous + incoming > WINDOW_ENTRIES;
        if !self.make_room(path, incoming, if append && !roll { 0 } else { previous }) {
            if let Some(node) = self.directories.get_mut(path) {
                node.loading = false;
                node.notice = Some("Collapse another folder before loading more items.".into());
            }
            self.rebuild();
            return false;
        }
        self.clock += 1;
        let node = self.directories.entry(path.to_path_buf()).or_default();
        if !append || roll {
            node.entries.clear();
            node.windowed = roll || (append && node.windowed);
        }
        node.entries.extend(entries);
        node.entries.sort_by(|left, right| {
            right
                .is_dir
                .cmp(&left.is_dir)
                .then_with(|| left.path.cmp(&right.path))
        });
        node.entries.dedup_by(|left, right| left.path == right.path);
        node.next = next;
        node.loaded = true;
        node.loading = false;
        node.stale = false;
        node.deferred = false;
        node.notice = None;
        node.revision = revision;
        node.touched = self.clock;
        self.prune_removed_children(path);
        self.rebuild();
        true
    }

    fn make_room(&mut self, target: &Path, incoming: usize, replacing: usize) -> bool {
        self.evict_collapsed();
        let mut retained: usize = self
            .directories
            .values()
            .map(|node| node.entries.len())
            .sum();
        while retained.saturating_sub(replacing) + incoming > MAX_ENTRIES {
            let candidate = self
                .directories
                .iter()
                .filter(|(path, node)| {
                    path.as_path() != target
                        && !target.starts_with(path)
                        && !node.loading
                        && !node.entries.is_empty()
                })
                .min_by_key(|(_, node)| node.touched)
                .map(|(path, _)| path.clone());
            let Some(candidate) = candidate else {
                return false;
            };
            let node = self
                .directories
                .get_mut(&candidate)
                .expect("selected directory exists");
            retained -= node.entries.len();
            node.entries.clear();
            node.next = None;
            node.loaded = false;
            node.windowed = false;
            node.deferred = true;
            // Refill is an explicit inline action, avoiding an automatic cycle
            // through many expanded directories at the memory bound.
        }
        true
    }

    fn evict_collapsed(&mut self) {
        if self.directories.len() < MAX_DIRECTORIES {
            return;
        }
        let visible: BTreeSet<_> = self.visible_directories().into_iter().collect();
        let candidate = self
            .directories
            .iter()
            .filter(|(path, node)| !visible.contains(*path) && !node.loading)
            .min_by_key(|(_, node)| node.touched)
            .map(|(path, _)| path.clone());
        if let Some(path) = candidate {
            self.directories
                .retain(|entry, _| !entry.starts_with(&path));
            self.expanded.retain(|entry| !entry.starts_with(&path));
        }
    }

    fn prune_removed_children(&mut self, directory: &Path) {
        let Some(node) = self.directories.get(directory) else {
            return;
        };
        let children: BTreeSet<_> = node
            .entries
            .iter()
            .filter(|entry| entry.is_dir)
            .map(|entry| entry.path.clone())
            .collect();
        let retained_window = node.windowed || node.next.is_some();
        // A partial page cannot prove an omitted child was deleted.
        if retained_window {
            return;
        }
        let removed: Vec<_> = self
            .directories
            .keys()
            .filter(|path| path.parent() == Some(directory) && !children.contains(*path))
            .cloned()
            .collect();
        for parent in removed {
            self.directories
                .retain(|path, _| !path.starts_with(&parent));
            self.expanded.retain(|path| !path.starts_with(&parent));
        }
    }

    pub fn rebuild(&mut self) {
        let mut rows = Vec::new();
        if let Some(root) = &self.root {
            self.append_entry(
                FileEntry {
                    path: root.clone(),
                    is_dir: true,
                    bytes: 0,
                },
                0,
                &mut rows,
            );
        }
        if self
            .selected
            .as_ref()
            .is_some_and(|key| !rows.iter().any(|row| row.key.as_ref() == Some(key)))
        {
            let selected_path = match &self.selected {
                Some(
                    RowKey::Entry(path)
                    | RowKey::More(path)
                    | RowKey::Earlier(path)
                    | RowKey::Load(path),
                ) => Some(path.clone()),
                None => None,
            };
            self.selected = selected_path.and_then(|mut path| {
                loop {
                    if rows
                        .iter()
                        .any(|row| row.key.as_ref() == Some(&RowKey::Entry(path.clone())))
                    {
                        break Some(RowKey::Entry(path));
                    }
                    if !path.pop() {
                        break None;
                    }
                }
            });
        }
        self.rows = Arc::new(rows);
    }

    fn append_entry(&self, entry: FileEntry, depth: usize, rows: &mut Vec<TreeRow>) {
        let path = entry.path.clone();
        let expanded = entry.is_dir && self.expanded.contains(&path);
        rows.push(TreeRow {
            path: path.clone(),
            depth,
            expanded,
            key: Some(RowKey::Entry(path.clone())),
            content: RowContent::Entry(entry),
        });
        if !expanded || depth >= MAX_DEPTH {
            return;
        }
        let Some(node) = self.directories.get(&path) else {
            rows.push(TreeRow {
                path: path.clone(),
                depth: depth + 1,
                expanded: false,
                key: Some(RowKey::Load(path)),
                content: RowContent::Load,
            });
            return;
        };
        if node.windowed {
            rows.push(TreeRow {
                path: path.clone(),
                depth: depth + 1,
                expanded: false,
                key: Some(RowKey::Earlier(path.clone())),
                content: RowContent::Earlier,
            });
        }
        for child in &node.entries {
            self.append_entry(child.clone(), depth + 1, rows);
        }
        if node.loading {
            rows.push(TreeRow {
                path: path.clone(),
                depth: depth + 1,
                expanded: false,
                key: None,
                content: RowContent::Loading,
            });
        } else if node.next.is_some() {
            rows.push(TreeRow {
                path: path.clone(),
                depth: depth + 1,
                expanded: false,
                key: Some(RowKey::More(path.clone())),
                content: RowContent::More,
            });
        } else if !node.loaded || node.notice.is_some() {
            rows.push(TreeRow {
                path: path.clone(),
                depth: depth + 1,
                expanded: false,
                key: Some(RowKey::Load(path.clone())),
                content: RowContent::Load,
            });
        } else if node.entries.is_empty() {
            rows.push(TreeRow {
                path: path.clone(),
                depth: depth + 1,
                expanded: false,
                key: None,
                content: RowContent::Notice("Empty folder".into()),
            });
        }
        if let Some(message) = &node.notice {
            rows.push(TreeRow {
                path,
                depth: depth + 1,
                expanded: false,
                key: None,
                content: RowContent::Notice(message.clone()),
            });
        }
    }
}
