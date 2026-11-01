//! Bounded output-style discovery using the workspace's confined file reader.
use anyhow::{Context, Result, ensure};
use std::io::Read;
use std::path::{Path, PathBuf};

/// One available style; its stable key identifies the scope and file name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputStyle {
    pub key: String,
    pub label: String,
    pub source: Option<PathBuf>,
    pub prompt: String,
}

/// Discovers styles for the current OS user and the selected project.
pub fn current_catalog(workspace: &Path) -> Result<Vec<OutputStyle>> {
    let home = directories::BaseDirs::new().context("Locate the user home directory")?;
    discover(workspace, home.home_dir())
}

/// Loads a selection using the current OS user's home directory.
pub fn current(workspace: &Path, key: &str) -> Result<OutputStyle> {
    let home = directories::BaseDirs::new().context("Locate the user home directory")?;
    selected(workspace, home.home_dir(), key)
}

/// Lists built-in choices and readable Markdown styles from both configured scopes.
pub fn discover(workspace: &Path, user_home: &Path) -> Result<Vec<OutputStyle>> {
    let mut styles: Vec<_> = [
        ("default", "Default"),
        ("explanatory", "Explanatory"),
        ("learning", "Learning"),
    ]
    .into_iter()
    .map(|(key, label)| OutputStyle {
        key: key.into(),
        label: label.into(),
        source: None,
        prompt: String::new(),
    })
    .collect();
    for (scope, root) in [("project", workspace), ("user", user_home)] {
        let directory = root.join(".xylo/output-styles");
        let Ok(entries) = std::fs::read_dir(directory) else {
            continue;
        };
        let mut names: Vec<_> = entries
            .take(256)
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
            .filter_map(|name| name.strip_suffix(".md").map(str::to_owned))
            .filter(|name| valid_name(name))
            .collect();
        names.sort();
        for name in names.into_iter().take(64) {
            if let Ok(style) = custom(root, scope, &name) {
                styles.push(style);
            }
        }
    }
    Ok(styles)
}

/// Loads exactly the selected style, refusing missing or unsafe custom files.
pub fn selected(workspace: &Path, user_home: &Path, key: &str) -> Result<OutputStyle> {
    if let Some(label) = match key {
        "default" => Some("Default"),
        "explanatory" => Some("Explanatory"),
        "learning" => Some("Learning"),
        _ => None,
    } {
        return Ok(OutputStyle {
            key: key.into(),
            label: label.into(),
            source: None,
            prompt: String::new(),
        });
    }
    let (scope, name) = key
        .split_once(':')
        .context("Choose an available output style")?;
    ensure!(valid_name(name), "Invalid output style name");
    let root = match scope {
        "project" => workspace,
        "user" => user_home,
        _ => anyhow::bail!("Invalid output style scope"),
    };
    custom(root, scope, name)
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !matches!(name, "." | "..")
        && !name
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\' | ':'))
}

fn custom(root: &Path, scope: &str, name: &str) -> Result<OutputStyle> {
    let root = root.canonicalize()?;
    let path = root.join(".xylo/output-styles").join(format!("{name}.md"));
    let cancel = Default::default();
    let reader = super::reader::RootReader::open(&root, &cancel, &cancel)?;
    let file = reader.file(&path, &cancel, &cancel)?;
    let mut bytes = Vec::new();
    file.take(16 * 1024 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 16 * 1024,
        "Output styles must be at most 16 KiB"
    );
    let prompt = String::from_utf8(bytes).context("Output style must be UTF-8 Markdown")?;
    ensure!(!prompt.trim().is_empty(), "Output style is empty");
    let label = prompt
        .lines()
        .find_map(|line| line.strip_prefix("# "))
        .unwrap_or(name)
        .trim()
        .chars()
        .take(80)
        .collect();
    Ok(OutputStyle {
        key: format!("{scope}:{name}"),
        label,
        source: Some(path),
        prompt,
    })
}

#[cfg(test)]
mod tests;
