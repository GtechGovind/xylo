//! Source discovery and pinned downloads. Network operations are explicit and
//! never happen while rendering the UI or constructing the service.
use super::*;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    time::Duration,
};

#[path = "marketplace.rs"]
mod marketplace;
pub use marketplace::{
    DIRECTORY_TTL_SECONDS, DirectoryEndpoints, DirectoryEntry, DirectoryFreshness, DirectoryPage,
    DirectoryQuery, DirectorySourceStatus,
};

/// A GitHub repository (optionally one folder of it) offering extensions.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExtensionSource {
    pub name: String,
    pub repository: String,
    pub revision: String,
    #[serde(default)]
    pub subdirectory: String,
}
impl ExtensionSource {
    pub fn from_input(input: &str) -> Result<Self> {
        let input = input.trim().trim_end_matches('/');
        let path = if input.starts_with("https://") {
            let url = reqwest::Url::parse(input)?;
            ensure!(
                url.host_str() == Some("github.com")
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.port().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none(),
                "Use a public GitHub repository or directory URL"
            );
            url.path().trim_matches('/').to_owned()
        } else {
            input.to_owned()
        };
        let parts = path.split('/').collect::<Vec<_>>();
        ensure!(
            parts.len() >= 2,
            "Use owner/repository or a GitHub directory URL"
        );
        let repository = format!("{}/{}", parts[0], parts[1].trim_end_matches(".git"));
        ensure!(repository_valid(&repository), "Invalid GitHub repository");
        let (revision, subdirectory) = if parts.len() == 2 {
            ("HEAD".into(), String::new())
        } else {
            ensure!(
                parts.len() >= 4 && parts[2] == "tree" && !parts[3].is_empty(),
                "Use a GitHub /tree/revision/directory URL"
            );
            (parts[3].to_owned(), parts[4..].join("/"))
        };
        ensure!(
            directory_valid(&subdirectory) && !path.contains('%'),
            "Use an unescaped package directory path"
        );
        Ok(Self {
            name: if subdirectory.is_empty() {
                repository.clone()
            } else {
                format!("{repository}/{subdirectory}")
            },
            repository,
            revision,
            subdirectory,
        })
    }
}
fn directory_valid(path: &str) -> bool {
    path.is_empty()
        || (path.len() <= 1024
            && path
                .split('/')
                .all(|p| !p.is_empty() && p != "." && p != ".." && !p.contains('\\')))
}
/// A package that can be installed from a pinned GitHub commit.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DiscoveredExtension {
    pub source: ExtensionSource,
    /// Package folder inside the repository, `.` for the root.
    pub path: String,
    pub name: String,
    /// Full commit SHA the package is installed from.
    pub revision: String,
    /// Identity of the package's content (its git tree hash, or the pinned
    /// commit of an external plugin), used to detect updates. `None` when
    /// unknown.
    pub content: Option<String>,
}
pub fn default_sources() -> Vec<ExtensionSource> {
    vec![
        ExtensionSource {
            name: "OpenAI plugins".into(),
            repository: "openai/plugins".into(),
            revision: "main".into(),
            subdirectory: String::new(),
        },
        ExtensionSource {
            name: "Claude plugins".into(),
            repository: "anthropics/claude-plugins-official".into(),
            revision: "main".into(),
            subdirectory: String::new(),
        },
    ]
}
fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent("Xylo-extensions/0.1")
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .build()?)
}
pub(super) fn repository_valid(repo: &str) -> bool {
    let p: Vec<_> = repo.split('/').collect();
    p.len() == 2
        && p.iter().all(|p| {
            !p.is_empty()
                && *p != "."
                && *p != ".."
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        })
}
pub(super) fn packages_from_tree(
    tree: &serde_json::Value,
    source: &ExtensionSource,
    revision: &str,
) -> Result<Vec<DiscoveredExtension>> {
    ensure!(
        tree["truncated"] != true,
        "Source tree is too large; add a narrower source"
    );
    let files = tree["tree"].as_array().context("Source tree missing")?;
    let scoped_files = files
        .iter()
        .filter(|file| {
            source.subdirectory.is_empty()
                || file["path"]
                    .as_str()
                    .is_some_and(|path| path.starts_with(&format!("{}/", source.subdirectory)))
        })
        .collect::<Vec<_>>();
    let files = &scoped_files;
    let mut packages = std::collections::BTreeMap::new();
    for file in files {
        if file["path"].as_str() == Some("SKILL.md") {
            packages.entry(".".into()).or_insert_with(|| {
                source
                    .repository
                    .rsplit('/')
                    .next()
                    .unwrap_or("skill")
                    .to_string()
            });
        }
        if let Some(path) = file["path"].as_str() {
            for suffix in [
                "/.codex-plugin/plugin.json",
                "/.claude-plugin/plugin.json",
                "/.agent-plugin/plugin.json",
            ] {
                if path == &suffix[1..] {
                    packages.insert(
                        ".".into(),
                        source
                            .repository
                            .rsplit('/')
                            .next()
                            .unwrap_or("plugin")
                            .to_string(),
                    );
                }
                if let Some(root) = path.strip_suffix(suffix) {
                    packages.insert(
                        root.to_string(),
                        root.rsplit('/').next().unwrap_or(root).to_owned(),
                    );
                }
            }
        }
    }
    for file in files {
        if file["path"].as_str() == Some("SKILL.md") {
            packages.entry(".".into()).or_insert_with(|| {
                source
                    .repository
                    .rsplit('/')
                    .next()
                    .unwrap_or("skill")
                    .to_string()
            });
        }
        if let Some(path) = file["path"]
            .as_str()
            .and_then(|p| p.strip_suffix("/SKILL.md"))
            && !packages
                .keys()
                .any(|root| root == "." || path.starts_with(&format!("{root}/")))
        {
            packages.insert(path.into(), path.rsplit('/').next().unwrap_or(path).into());
        }
    }
    Ok(packages
        .into_iter()
        .filter(|(path, _)| {
            source.subdirectory.is_empty()
                || path == &source.subdirectory
                || path.starts_with(&format!("{}/", source.subdirectory))
        })
        .map(|(path, name)| DiscoveredExtension {
            source: source.clone(),
            path,
            name,
            revision: revision.into(),
            content: None,
        })
        .collect())
}
impl ExtensionService {
    /// Configured sources, or the defaults. A sources file that does not
    /// decode is deleted and the defaults apply.
    pub fn sources(&self) -> Result<Vec<ExtensionSource>> {
        let _lock = self.lock()?;
        let path = self.root.join("sources.json");
        match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(sources) => Ok(sources),
                Err(_) => {
                    std::fs::remove_file(&path)?;
                    Ok(default_sources())
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(default_sources()),
            Err(e) => Err(e.into()),
        }
    }
    pub fn save_sources(&self, sources: &[ExtensionSource]) -> Result<()> {
        ensure!(sources.len() <= 32, "Maximum 32 sources");
        for source in sources {
            ensure!(
                repository_valid(&source.repository)
                    && !source.revision.is_empty()
                    && source.revision.len() <= 256,
                "Invalid source"
            );
            ensure!(
                directory_valid(&source.subdirectory),
                "Invalid source directory"
            );
        }
        let _lock = self.lock()?;
        let mut file = tempfile::NamedTempFile::new_in(&self.root)?;
        file.write_all(&serde_json::to_vec_pretty(sources)?)?;
        file.as_file().sync_all()?;
        file.persist(self.root.join("sources.json"))?;
        Ok(())
    }
    pub async fn install_discovered(&self, item: &DiscoveredExtension) -> Result<ExtensionPackage> {
        ensure!(
            repository_valid(&item.source.repository),
            "Invalid repository"
        );
        ensure!(
            item.revision.len() == 40 && item.revision.chars().all(|c| c.is_ascii_hexdigit()),
            "Installation requires a pinned commit"
        );
        ensure!(
            item.path == "."
                || (!item.path.is_empty()
                    && item
                        .path
                        .split('/')
                        .all(|p| !p.is_empty() && p != "." && p != "..")),
            "Invalid package path"
        );
        let url = format!(
            "https://api.github.com/repos/{}/zipball/{}",
            item.source.repository, item.revision
        );
        let mut response = client()?.get(url).send().await?.error_for_status()?;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            ensure!(
                bytes.len() + chunk.len() <= 128 * 1024 * 1024,
                "Source archive too large"
            );
            bytes.extend_from_slice(&chunk);
        }
        let service = self.clone();
        let item = item.clone();
        let installed = tokio::task::spawn_blocking(move || {
            let staging = tempfile::tempdir()?;
            let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))?;
            let mut total = 0u64;
            let mut count = 0;
            for i in 0..zip.len() {
                let mut file = zip.by_index(i)?;
                let enclosed = file.enclosed_name().context("Unsafe archive path")?;
                let relative: std::path::PathBuf = enclosed.components().skip(1).collect();
                let Ok(relative) =
                    relative.strip_prefix(if item.path == "." { "" } else { &item.path })
                else {
                    continue;
                };
                if relative.as_os_str().is_empty() {
                    continue;
                }
                ensure!(!file.is_symlink(), "Symlinks are not supported");
                count += 1;
                ensure!(count <= 10_000, "Too many package files");
                total += file.size();
                ensure!(total <= 64 * 1024 * 1024, "Package exceeds 64 MiB");
                let path = staging.path().join(relative);
                if file.is_dir() {
                    std::fs::create_dir_all(path)?;
                } else {
                    std::fs::create_dir_all(path.parent().unwrap())?;
                    let mut data = Vec::new();
                    file.by_ref()
                        .take(64 * 1024 * 1024 + 1)
                        .read_to_end(&mut data)?;
                    ensure!(data.len() as u64 <= file.size(), "Invalid archive length");
                    std::fs::File::create(&path)?.write_all(&data)?;
                    #[cfg(unix)]
                    if let Some(mode) = file.unix_mode() {
                        use std::os::unix::fs::PermissionsExt;
                        std::fs::set_permissions(
                            &path,
                            std::fs::Permissions::from_mode(mode & 0o755),
                        )?;
                    }
                }
            }
            service
                .install_snapshot(
                    staging.path(),
                    &format!(
                        "https://github.com/{}/tree/{}/{}",
                        item.source.repository, item.source.revision, item.path
                    ),
                    Some(item.revision.clone()),
                    if item.path == "." {
                        item.source
                            .repository
                            .rsplit('/')
                            .next()
                            .unwrap_or("plugin")
                    } else {
                        item.path.rsplit('/').next().unwrap_or("plugin")
                    },
                )
                .map(|package| (package, item))
        })
        .await??;
        let (package, item) = installed;
        self.record_installed_content(&item)?;
        Ok(package)
    }
}
