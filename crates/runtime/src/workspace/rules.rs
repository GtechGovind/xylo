//! Point-event ignore checks without enumerating every sibling. The initial
//! walker remains ignore's standard traversal. This small FIFO cache stores at
//! most 64 ancestor rule chains, never one entry per project directory.
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
};

struct Rule {
    root: PathBuf,
    matcher: Gitignore,
}
struct Chain {
    parent: PathBuf,
    ignores: Vec<Rule>,
    git: Vec<Rule>,
    excludes: Vec<Rule>,
}
pub(super) struct Visibility {
    root: PathBuf,
    global: Gitignore,
    chains: VecDeque<Chain>,
}
impl Visibility {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_owned(),
            global: Gitignore::global().0,
            chains: VecDeque::new(),
        }
    }
    pub fn visible(&mut self, path: &Path, is_dir: bool) -> bool {
        if !path.starts_with(&self.root)
            || path
                .strip_prefix(&self.root)
                .is_ok_and(|relative| relative.components().any(|part| part.as_os_str() == ".git"))
        {
            return false;
        }
        let parent = path.parent().unwrap_or(&self.root);
        let position = self.chains.iter().position(|chain| chain.parent == parent);
        let chain = if let Some(position) = position {
            &self.chains[position]
        } else {
            if self.chains.len() == 64 {
                self.chains.pop_front();
            }
            self.chains.push_back(Chain::new(parent));
            self.chains.back().expect("Just inserted ignore chain")
        };
        // An ignored directory cannot be re-included merely by a child pattern.
        let relative = path
            .strip_prefix(&self.root)
            .expect("Checked project prefix");
        let mut current = self.root.clone();
        let length = relative.components().count();
        for (position, part) in relative.components().enumerate() {
            current.push(part.as_os_str());
            let directory = position + 1 < length || is_dir;
            let decision = matches(&chain.ignores, &current, directory)
                .or_else(|| matches(&chain.git, &current, directory))
                .or_else(|| matches(&chain.excludes, &current, directory))
                .unwrap_or_else(|| self.global.matched(&current, directory).is_ignore());
            if decision {
                return false;
            }
        }
        true
    }
}
impl Chain {
    fn new(parent: &Path) -> Self {
        let mut chain = Self {
            parent: parent.to_owned(),
            ignores: Vec::new(),
            git: Vec::new(),
            excludes: Vec::new(),
        };
        let mut git_boundary = false;
        for directory in parent.ancestors() {
            add(&mut chain.ignores, directory, ".ignore");
            if !git_boundary {
                add(&mut chain.git, directory, ".gitignore");
                if let Some(common) = git_common(directory) {
                    add_path(&mut chain.excludes, directory, &common.join("info/exclude"));
                }
            }
            git_boundary |= directory.join(".git").exists() || directory.join(".jj").exists();
        }
        chain
    }
}
fn add(rules: &mut Vec<Rule>, directory: &Path, name: &str) {
    let path = directory.join(name);
    add_path(rules, directory, &path);
}
fn add_path(rules: &mut Vec<Rule>, directory: &Path, path: &Path) {
    if path.is_file() {
        let mut builder = GitignoreBuilder::new(directory);
        let _ = builder.add(path);
        if let Ok(matcher) = builder.build() {
            rules.push(Rule {
                root: directory.to_owned(),
                matcher,
            });
        }
    }
}
/// Same gitfile/commondir convention as pinned ignore0.4, without invoking git
/// or retaining a rule entry for every worktree in the selected project.
fn git_common(directory: &Path) -> Option<PathBuf> {
    let git = directory.join(".git");
    if !git.is_file() {
        return Some(git);
    }
    let first = first_line(&git)?;
    let target = PathBuf::from(first.strip_prefix("gitdir: ")?);
    let common = first_line(&target.join("commondir"))?;
    Some(if common.starts_with('.') {
        target.join(common)
    } else {
        PathBuf::from(common)
    })
}
fn first_line(path: &Path) -> Option<String> {
    use std::io::{BufRead, BufReader, Read};
    let file = std::fs::File::open(path).ok()?;
    let mut line = String::new();
    BufReader::new(file.take(128 * 1024 + 1))
        .read_line(&mut line)
        .ok()?;
    if line.len() > 128 * 1024 {
        return None;
    }
    Some(line.trim_end_matches(['\r', '\n']).to_owned())
}
fn matches(rules: &[Rule], path: &Path, directory: bool) -> Option<bool> {
    for rule in rules {
        if path.starts_with(&rule.root) {
            let matched = rule.matcher.matched(path, directory);
            if !matched.is_none() {
                return Some(matched.is_ignore());
            }
        }
    }
    None
}
