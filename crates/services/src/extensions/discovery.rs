//! Project and user skill sources, copied into the existing reviewed snapshot catalog.
use super::{ExtensionPackage, ExtensionService};
use anyhow::Result;
use std::path::{Path, PathBuf};

impl ExtensionService {
    /// Restricts discovered project packages to the current canonical workspace.
    pub fn for_workspace(mut self, workspace: &Path) -> Self {
        self.workspace = Some(
            workspace
                .canonicalize()
                .unwrap_or_else(|_| workspace.to_owned()),
        );
        self
    }
    pub(super) fn in_scope(&self, package: &ExtensionPackage) -> bool {
        let Some(path) = package.source.strip_prefix("project:") else {
            return true;
        };
        self.workspace.as_ref().is_none_or(|workspace| {
            [".claude", ".xylo"]
                .iter()
                .any(|folder| path == format!("{}/{folder}", workspace.display()))
        })
    }
    /// Finds instruction packages without running code or enabling new/changed content.
    /// Only skills and commands are snapshotted; agents belong to their separate discovery path.
    pub fn discover_project(
        &self,
        workspace: &Path,
        home: Option<&Path>,
    ) -> Result<Vec<ExtensionPackage>> {
        let workspace = workspace.canonicalize()?;
        let mut roots = vec![(workspace.clone(), "project")];
        if let Some(home) = home.and_then(|home| home.canonicalize().ok()) {
            roots.push((home, "user"));
        }
        let mut output = Vec::new();
        let mut failure = None;
        for (root, scope) in roots {
            for folder in [".claude", ".xylo"] {
                let source = root.join(folder);
                if !source.is_dir() {
                    continue;
                }
                let discovered = (|| -> Result<Option<ExtensionPackage>> {
                    let staging = tempfile::tempdir()?;
                    let mut total = 0;
                    let mut count = 0;
                    for kind in ["skills", "commands"] {
                        let directory = source.join(kind);
                        if !directory.is_dir() {
                            continue;
                        }
                        let canonical = directory.canonicalize()?;
                        anyhow::ensure!(
                            canonical.starts_with(&root),
                            "Project package directory leaves its source root"
                        );
                        super::catalog::copy_tree(
                            &canonical,
                            &staging.path().join(kind),
                            &mut total,
                            &mut count,
                            0,
                        )?;
                    }
                    if count == 0 {
                        return Ok(None);
                    }
                    self.install_snapshot(
                        staging.path(),
                        &format!("{scope}:{}/{}", root.display(), folder),
                        None,
                        &format!(
                            "{} {}",
                            root.file_name().unwrap_or_default().to_string_lossy(),
                            folder
                        ),
                    )
                    .map(Some)
                })();
                match discovered {
                    Ok(Some(package)) => output.push(package),
                    Ok(None) => {}
                    Err(error) => {
                        failure.get_or_insert(error);
                    }
                }
            }
        }
        let current = output
            .iter()
            .map(|package| package.source.as_str())
            .collect::<std::collections::HashSet<_>>();
        for package in self.list()? {
            if package.enabled
                && (package
                    .source
                    .starts_with(&format!("project:{}/", workspace.display()))
                    || (home.is_some() && package.source.starts_with("user:")))
                && !current.contains(package.source.as_str())
            {
                self.set_enabled(&package.id, false)?;
            }
        }
        if let Some(error) = failure {
            return Err(error);
        }
        Ok(output)
    }
    /// Discovers the conventional home and the selected workspace, without enabling packages.
    pub fn discover_workspace(&self, workspace: &Path) -> Result<Vec<ExtensionPackage>> {
        self.discover_project(
            workspace,
            std::env::var_os("HOME").map(PathBuf::from).as_deref(),
        )
    }
}
