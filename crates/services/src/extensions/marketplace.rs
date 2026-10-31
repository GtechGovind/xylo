//! Rich, cached extension directories built from marketplace indexes.
//!
//! Each configured GitHub source is read through its marketplace index
//! (`.claude-plugin/marketplace.json` for Claude marketplaces,
//! `.agents/plugins/marketplace.json` for the Codex plugin index). A plugin's
//! own manifest (`.codex-plugin/plugin.json`, `.claude-plugin/plugin.json`,
//! `.agent-plugin/plugin.json`) or `SKILL.md` frontmatter only fills fields the
//! index leaves empty. Repositories without an index fall back to the package
//! layout found in the source tree.
//!
//! The result is cached on disk per source under `<extensions>/directory/`.
//! [`ExtensionService::directory`] answers instantly from the cache, so the
//! UI can search as the user types; [`ExtensionService::refresh_directory`]
//! revalidates with `If-None-Match` and a 6 hour time-to-live. A refresh costs
//! two GitHub API requests when the source changed and one (answered 304,
//! which GitHub does not count) when it did not. Index and manifest files come
//! from `raw.githubusercontent.com`, outside the API rate limit, and manifests
//! are cached by blob hash. When GitHub reports its rate limit, the cached
//! directory is kept and its status says so.
use super::*;
use anyhow::{Context, Result, bail, ensure};
use futures_util::{StreamExt, stream};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    path::PathBuf,
    time::Duration,
};

/// How long a fetched directory counts as fresh.
pub const DIRECTORY_TTL_SECONDS: i64 = 6 * 60 * 60;
const MAX_INDEX_BYTES: usize = 4 * 1024 * 1024;
const MAX_MANIFEST_BYTES: usize = 256 * 1024;
/// GitHub truncates a recursive tree near 7 MB; anything larger is not a tree.
const MAX_TREE_BYTES: usize = 8 * 1024 * 1024;
const MAX_ENTRIES: usize = 2000;
const MANIFEST_CONCURRENCY: usize = 8;
const RATE_LIMITED: &str = "Showing cached results; GitHub rate limit reached";

/// One installable (or listed) extension in a source's directory.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DirectoryEntry {
    /// Stable identity within all sources: `<repository>#<name>`.
    pub id: String,
    /// Machine name from the index or manifest.
    pub name: String,
    pub display_name: String,
    /// One line; empty when neither the index nor the manifest has one.
    pub description: String,
    pub version: Option<String>,
    /// Author, developer or publisher name.
    pub author: Option<String>,
    pub homepage: Option<String>,
    pub categories: Vec<String>,
    pub keywords: Vec<String>,
    /// Component kinds the package declares or contains.
    pub components: Vec<ComponentKind>,
    /// Display name of the source this entry came from.
    pub source: String,
    /// What to pass to `install_discovered`; `None` when Xylo cannot install
    /// it (see `unavailable`).
    pub install: Option<DiscoveredExtension>,
    /// Why the entry cannot be installed, when it cannot.
    pub unavailable: Option<String>,
    /// Filled per query from the installed catalog.
    pub installed: bool,
    /// Installed, and the directory offers different content.
    pub update_available: bool,
}

/// How current a source's cached directory is.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DirectoryFreshness {
    /// Never fetched; no entries.
    Missing,
    /// Fetched or revalidated within the time-to-live.
    Fresh,
    /// Older than the time-to-live; a refresh is due.
    Stale,
    /// The last refresh hit GitHub's rate limit; cached entries are shown.
    RateLimited,
    /// The last refresh failed for another reason; cached entries are shown.
    Failed,
}

/// Per-source status shown next to directory results.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DirectorySourceStatus {
    pub source: ExtensionSource,
    pub freshness: DirectoryFreshness,
    /// Unix seconds of the last successful fetch or revalidation.
    pub fetched_at: Option<i64>,
    /// Pinned commit the entries were read from.
    pub revision: Option<String>,
    pub entries: usize,
    /// User-facing explanation for `RateLimited` and `Failed`.
    pub message: Option<String>,
}

/// A local search over cached directories.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DirectoryQuery {
    /// Words that must all appear in the name, display name, description,
    /// author, categories or keywords (case-insensitive). Empty matches all.
    pub text: String,
    /// Limit to one source by repository (`owner/name`).
    pub source: Option<String>,
    /// Limit to one category (case-insensitive).
    pub category: Option<String>,
    pub installed_only: bool,
    pub offset: usize,
    /// Page size; 0 means 50. At most 200.
    pub limit: usize,
}

/// One page of directory results.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DirectoryPage {
    pub entries: Vec<DirectoryEntry>,
    /// Matches before paging.
    pub total: usize,
    pub offset: usize,
    /// Every category present in the matching sources, sorted.
    pub categories: Vec<String>,
    pub sources: Vec<DirectorySourceStatus>,
}

/// Where directory data is fetched from. Tests point both at a local server.
#[derive(Clone, Debug)]
pub struct DirectoryEndpoints {
    pub api: String,
    pub raw: String,
}
impl Default for DirectoryEndpoints {
    fn default() -> Self {
        Self {
            api: "https://api.github.com".into(),
            raw: "https://raw.githubusercontent.com".into(),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cached {
    source_key: String,
    etag: Option<String>,
    revision: Option<String>,
    fetched_at: Option<i64>,
    freshness_note: Option<(DirectoryFreshness, String)>,
    entries: Vec<DirectoryEntry>,
    /// Manifest details by git blob hash, reused across refreshes.
    manifests: BTreeMap<String, ManifestDetails>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct ManifestDetails {
    display_name: Option<String>,
    description: Option<String>,
    version: Option<String>,
    author: Option<String>,
    homepage: Option<String>,
    categories: Vec<String>,
    keywords: Vec<String>,
}

/// GitHub answered with its rate-limit refusal.
#[derive(Debug)]
struct RateLimited;
impl std::fmt::Display for RateLimited {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        output.write_str(RATE_LIMITED)
    }
}
impl std::error::Error for RateLimited {}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn source_key(source: &ExtensionSource) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(
        format!(
            "{}\0{}\0{}",
            source.repository, source.revision, source.subdirectory
        )
        .as_bytes(),
    ))
}

/// The provenance `install_discovered` records, used to match installed
/// packages to directory entries.
pub(super) fn provenance(item: &DiscoveredExtension) -> String {
    format!(
        "https://github.com/{}/tree/{}/{}",
        item.source.repository, item.source.revision, item.path
    )
}

impl ExtensionService {
    fn directory_root(&self) -> PathBuf {
        self.root.join("directory")
    }
    fn cache_path(&self, source: &ExtensionSource) -> PathBuf {
        self.directory_root()
            .join(format!("{}.json", source_key(source)))
    }
    /// A cache that does not decode (corrupt or an older format) is deleted.
    fn read_cache(&self, source: &ExtensionSource) -> Result<Cached> {
        let path = self.cache_path(source);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Cached::default()),
            Err(e) => return Err(e.into()),
        };
        match serde_json::from_slice::<Cached>(&bytes) {
            Ok(cached) if cached.source_key == source_key(source) => Ok(cached),
            _ => {
                let _ = std::fs::remove_file(&path);
                Ok(Cached::default())
            }
        }
    }
    fn write_cache(&self, source: &ExtensionSource, cached: &Cached) -> Result<()> {
        std::fs::create_dir_all(self.directory_root())?;
        let mut file = tempfile::NamedTempFile::new_in(self.directory_root())?;
        file.write_all(&serde_json::to_vec(cached)?)?;
        file.as_file().sync_all()?;
        file.persist(self.cache_path(source))?;
        Ok(())
    }
    fn installed_content_path(&self) -> PathBuf {
        self.directory_root().join("installed-content.json")
    }
    fn installed_content(&self) -> BTreeMap<String, String> {
        std::fs::read(self.installed_content_path())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }
    /// Remember which directory content an installed package came from, so a
    /// later directory change can be offered as an update.
    pub(super) fn record_installed_content(&self, item: &DiscoveredExtension) -> Result<()> {
        let Some(content) = &item.content else {
            return Ok(());
        };
        // Read-modify-write under the catalog lock so concurrent installs
        // cannot drop each other's records; replace atomically and durably.
        let _lock = self.lock()?;
        let mut map = self.installed_content();
        map.insert(provenance(item), content.clone());
        let root = self.directory_root();
        std::fs::create_dir_all(&root)?;
        let mut file = tempfile::NamedTempFile::new_in(&root)?;
        file.write_all(&serde_json::to_vec(&map)?)?;
        file.as_file().sync_all()?;
        file.persist(self.installed_content_path())?;
        #[cfg(unix)]
        std::fs::File::open(&root)?.sync_all()?;
        Ok(())
    }

    fn status(&self, source: &ExtensionSource, cached: &Cached) -> DirectorySourceStatus {
        let freshness = match (&cached.freshness_note, cached.fetched_at) {
            (Some((freshness, _)), _) => *freshness,
            (None, None) => DirectoryFreshness::Missing,
            (None, Some(at)) if now() - at > DIRECTORY_TTL_SECONDS => DirectoryFreshness::Stale,
            (None, Some(_)) => DirectoryFreshness::Fresh,
        };
        DirectorySourceStatus {
            source: source.clone(),
            freshness,
            fetched_at: cached.fetched_at,
            revision: cached.revision.clone(),
            entries: cached.entries.len(),
            message: cached
                .freshness_note
                .as_ref()
                .map(|(_, message)| message.clone()),
        }
    }

    /// Search the cached directories of the configured sources. No network
    /// access; answers in milliseconds, so it can run on every keystroke.
    pub fn directory(&self, query: &DirectoryQuery) -> Result<DirectoryPage> {
        let sources = self.sources()?;
        let packages = self.list()?;
        let installed_content = self.installed_content();
        let words: Vec<String> = query
            .text
            .split_whitespace()
            .map(str::to_lowercase)
            .collect();
        let mut statuses = Vec::new();
        let mut matches = Vec::new();
        let mut categories = BTreeSet::new();
        for source in sources.iter().filter(|source| {
            query
                .source
                .as_deref()
                .is_none_or(|repository| repository == source.repository)
        }) {
            let cached = self.read_cache(source)?;
            statuses.push(self.status(source, &cached));
            for mut entry in cached.entries {
                categories.extend(entry.categories.iter().cloned());
                if let Some(item) = &entry.install {
                    let origin = provenance(item);
                    if let Some(package) = packages.iter().find(|package| package.source == origin)
                    {
                        entry.installed = true;
                        entry.update_available =
                            match (&item.content, installed_content.get(&origin)) {
                                (Some(offered), Some(installed)) => offered != installed,
                                _ => package.revision.as_deref() != Some(item.revision.as_str()),
                            };
                    }
                }
                if query.installed_only && !entry.installed {
                    continue;
                }
                if let Some(category) = &query.category
                    && !entry
                        .categories
                        .iter()
                        .any(|value| value.eq_ignore_ascii_case(category))
                {
                    continue;
                }
                let haystack = [
                    entry.name.as_str(),
                    entry.display_name.as_str(),
                    entry.description.as_str(),
                    entry.author.as_deref().unwrap_or(""),
                ]
                .into_iter()
                .chain(entry.categories.iter().map(String::as_str))
                .chain(entry.keywords.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join("\n")
                .to_lowercase();
                if words.iter().all(|word| haystack.contains(word)) {
                    matches.push(entry);
                }
            }
        }
        // Name matches first, then alphabetical by display name.
        matches.sort_by_cached_key(|entry| {
            let name = entry.display_name.to_lowercase();
            (
                !words.iter().all(|word| name.contains(word)),
                name,
                entry.id.clone(),
            )
        });
        let total = matches.len();
        let limit = match query.limit {
            0 => 50,
            limit => limit.min(200),
        };
        Ok(DirectoryPage {
            entries: matches.into_iter().skip(query.offset).take(limit).collect(),
            total,
            offset: query.offset,
            categories: categories.into_iter().collect(),
            sources: statuses,
        })
    }

    /// Refresh every configured source whose cache is older than the TTL (or
    /// all of them with `force`). Never fails as a whole: each source's
    /// outcome is in its status.
    pub async fn refresh_directory(&self, force: bool) -> Result<Vec<DirectorySourceStatus>> {
        self.refresh_directory_from(&DirectoryEndpoints::default(), force)
            .await
    }

    /// [`Self::refresh_directory`] against explicit endpoints.
    pub async fn refresh_directory_from(
        &self,
        endpoints: &DirectoryEndpoints,
        force: bool,
    ) -> Result<Vec<DirectorySourceStatus>> {
        let mut statuses = Vec::new();
        for source in self.sources()? {
            statuses.push(self.refresh_source(endpoints, &source, force).await?);
        }
        Ok(statuses)
    }

    async fn refresh_source(
        &self,
        endpoints: &DirectoryEndpoints,
        source: &ExtensionSource,
        force: bool,
    ) -> Result<DirectorySourceStatus> {
        let mut cached = self.read_cache(source)?;
        let fresh = cached.freshness_note.is_none()
            && cached
                .fetched_at
                .is_some_and(|at| now() - at <= DIRECTORY_TTL_SECONDS);
        if fresh && !force {
            return Ok(self.status(source, &cached));
        }
        match fetch(endpoints, source, &cached).await {
            Ok(Fetched::Unchanged { etag }) => {
                cached.etag = etag.or(cached.etag);
                cached.fetched_at = Some(now());
                cached.freshness_note = None;
            }
            Ok(Fetched::Changed {
                etag,
                revision,
                entries,
                manifests,
            }) => {
                cached.etag = etag;
                cached.revision = Some(revision);
                cached.entries = entries;
                cached.manifests = manifests;
                cached.fetched_at = Some(now());
                cached.freshness_note = None;
            }
            Err(error) => {
                let note = if error.downcast_ref::<RateLimited>().is_some() {
                    (DirectoryFreshness::RateLimited, RATE_LIMITED.to_owned())
                } else {
                    (
                        DirectoryFreshness::Failed,
                        format!(
                            "Showing cached results; could not refresh {}: {error:#}",
                            source.name
                        ),
                    )
                };
                cached.freshness_note = Some(note);
            }
        }
        cached.source_key = source_key(source);
        self.write_cache(source, &cached)?;
        Ok(self.status(source, &cached))
    }
}

enum Fetched {
    Unchanged {
        etag: Option<String>,
    },
    Changed {
        etag: Option<String>,
        revision: String,
        entries: Vec<DirectoryEntry>,
        manifests: BTreeMap<String, ManifestDetails>,
    },
}

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent("Xylo-extensions/0.1")
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .build()?)
}

fn rate_limited(response: &reqwest::Response) -> bool {
    matches!(response.status().as_u16(), 403 | 429)
        && response
            .headers()
            .get("x-ratelimit-remaining")
            .is_some_and(|value| value == "0")
}

async fn checked(response: reqwest::Response) -> Result<reqwest::Response> {
    if rate_limited(&response) {
        return Err(RateLimited.into());
    }
    Ok(response.error_for_status()?)
}

/// Reads at most `maximum` bytes. The body is streamed and counted, so a
/// response without a Content-Length (chunked or close-delimited) is cut off
/// at the limit instead of being buffered whole.
async fn bounded_text(mut response: reqwest::Response, maximum: usize) -> Result<Vec<u8>> {
    ensure!(
        response
            .content_length()
            .is_none_or(|length| length <= maximum as u64),
        "Directory file exceeds its size limit"
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            bytes.len() + chunk.len() <= maximum,
            "Directory file exceeds its size limit"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn fetch(
    endpoints: &DirectoryEndpoints,
    source: &ExtensionSource,
    cached: &Cached,
) -> Result<Fetched> {
    let client = client()?;
    let mut request = client
        .get(format!(
            "{}/repos/{}/commits/{}",
            endpoints.api, source.repository, source.revision
        ))
        .header("Accept", "application/vnd.github.sha");
    if let (Some(etag), Some(_)) = (&cached.etag, &cached.revision) {
        request = request.header("If-None-Match", etag);
    }
    let response = request.send().await?;
    if response.status() == reqwest::StatusCode::NOT_MODIFIED {
        return Ok(Fetched::Unchanged {
            etag: etag_of(&response),
        });
    }
    let response = checked(response).await?;
    let etag = etag_of(&response);
    let revision = String::from_utf8(bounded_text(response, 256).await?)?
        .trim()
        .to_owned();
    ensure!(
        revision.len() == 40 && revision.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "Source commit is not a full SHA"
    );
    if cached.revision.as_deref() == Some(revision.as_str()) {
        return Ok(Fetched::Unchanged { etag });
    }
    let tree = bounded_text(
        checked(
            client
                .get(format!(
                    "{}/repos/{}/git/trees/{revision}",
                    endpoints.api, source.repository
                ))
                .query(&[("recursive", "1")])
                .send()
                .await?,
        )
        .await?,
        MAX_TREE_BYTES,
    )
    .await?;
    let tree: Value = serde_json::from_slice(&tree).context("Source tree is not JSON")?;
    let tree = Tree::new(&tree)?;
    let raw = |path: &str| format!("{}/{}/{revision}/{path}", endpoints.raw, source.repository);
    let index = [
        (".claude-plugin/marketplace.json", IndexFormat::Claude),
        (".agents/plugins/marketplace.json", IndexFormat::Codex),
    ]
    .into_iter()
    .find(|(path, _)| source.subdirectory.is_empty() && tree.blobs.contains_key(*path));
    let mut entries = match index {
        Some((path, format)) => {
            let bytes = bounded_text(
                checked(client.get(raw(path)).send().await?).await?,
                MAX_INDEX_BYTES,
            )
            .await?;
            let index: Value =
                serde_json::from_slice(&bytes).context("Marketplace index is not JSON")?;
            index_entries(&index, format, source, &revision, &tree)?
        }
        None => tree_entries(source, &revision, &tree)?,
    };
    ensure!(
        entries.len() <= MAX_ENTRIES,
        "Source lists too many extensions"
    );
    // Manifests fill only what the index left empty, and only for packages
    // inside this repository.
    let mut manifests = BTreeMap::new();
    let wanted: Vec<(usize, String, String)> = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| needs_manifest(&entry.0))
        .filter_map(|(index, entry)| {
            entry
                .1
                .as_ref()
                .map(|(path, blob)| (index, path.clone(), blob.clone()))
        })
        .collect();
    let fetched: Vec<(usize, String, Result<ManifestDetails>)> = stream::iter(wanted)
        .map(|(index, path, blob)| {
            let client = client.clone();
            let url = raw(&path);
            let known = cached.manifests.get(&blob).cloned();
            async move {
                let details = match known {
                    Some(details) => Ok(details),
                    None => {
                        async {
                            let bytes = bounded_text(
                                checked(client.get(url).send().await?).await?,
                                MAX_MANIFEST_BYTES,
                            )
                            .await?;
                            manifest_details(&path, &bytes)
                        }
                        .await
                    }
                };
                (index, blob, details)
            }
        })
        .buffer_unordered(MANIFEST_CONCURRENCY)
        .collect()
        .await;
    for (index, blob, details) in fetched {
        match details {
            Ok(details) => {
                fill(&mut entries[index].0, &details);
                manifests.insert(blob, details);
            }
            // A rate limit stops the refresh; other manifest failures only
            // leave that entry's optional fields empty.
            Err(error) if error.downcast_ref::<RateLimited>().is_some() => return Err(error),
            Err(_) => {}
        }
    }
    Ok(Fetched::Changed {
        etag,
        revision,
        entries: entries.into_iter().map(|(entry, _)| entry).collect(),
        manifests,
    })
}

fn etag_of(response: &reqwest::Response) -> Option<String> {
    response
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// Blob and subtree hashes of a recursive git tree.
struct Tree {
    blobs: BTreeMap<String, String>,
    trees: BTreeMap<String, String>,
}
impl Tree {
    fn new(value: &Value) -> Result<Self> {
        ensure!(
            value["truncated"] != true,
            "Source tree is too large; add a narrower source"
        );
        let mut blobs = BTreeMap::new();
        let mut trees = BTreeMap::new();
        for item in value["tree"].as_array().context("Source tree missing")? {
            let (Some(path), Some(sha)) = (item["path"].as_str(), item["sha"].as_str()) else {
                continue;
            };
            match item["type"].as_str() {
                Some("blob") => {
                    blobs.insert(path.to_owned(), sha.to_owned());
                }
                Some("tree") => {
                    trees.insert(path.to_owned(), sha.to_owned());
                }
                _ => {}
            }
        }
        Ok(Self { blobs, trees })
    }
    fn join(root: &str, path: &str) -> String {
        if root.is_empty() || root == "." {
            path.to_owned()
        } else {
            format!("{root}/{path}")
        }
    }
    /// Component kinds present in the package rooted at `root`.
    fn components(&self, root: &str) -> Vec<ComponentKind> {
        let has_dir = |name: &str| {
            let prefix = format!("{}/", Self::join(root, name));
            self.blobs.keys().any(|path| path.starts_with(&prefix))
        };
        let has_file = |name: &str| self.blobs.contains_key(&Self::join(root, name));
        let mut kinds = Vec::new();
        if has_dir("skills") || has_file("SKILL.md") {
            kinds.push(ComponentKind::Skill);
        }
        if has_dir("commands") {
            kinds.push(ComponentKind::Command);
        }
        if has_dir("agents") {
            kinds.push(ComponentKind::Agent);
        }
        if has_file("hooks/hooks.json") || has_file("hooks.json") {
            kinds.push(ComponentKind::Hooks);
        }
        if has_file(".mcp.json") {
            kinds.push(ComponentKind::Mcp);
        }
        if has_file(".lsp.json") {
            kinds.push(ComponentKind::LanguageServer);
        }
        if has_file(".app.json") {
            kinds.push(ComponentKind::HostedIntegration);
        }
        kinds
    }
    /// The manifest (or SKILL.md) describing the package at `root`, with its
    /// blob hash.
    fn manifest(&self, root: &str) -> Option<(String, String)> {
        [
            ".codex-plugin/plugin.json",
            ".claude-plugin/plugin.json",
            ".agent-plugin/plugin.json",
            "SKILL.md",
        ]
        .into_iter()
        .map(|name| Self::join(root, name))
        .find_map(|path| {
            self.blobs
                .get(&path)
                .map(|blob| (path.clone(), blob.clone()))
        })
    }
}

#[derive(Clone, Copy)]
enum IndexFormat {
    Claude,
    Codex,
}

type Pending = (DirectoryEntry, Option<(String, String)>);

fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|text| !text.is_empty())
}

fn person(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(name) => Some(name.clone()),
        Value::Object(fields) => fields
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_owned),
        _ => None,
    }
    .filter(|name| !name.trim().is_empty())
}

fn strings(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(one)) if !one.is_empty() => vec![one.clone()],
        Some(Value::Array(many)) => many
            .iter()
            .filter_map(Value::as_str)
            .filter(|one| !one.is_empty())
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

/// The GitHub repository named by a clone URL, if it is one.
fn github_repository(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://github.com/")?;
    let mut parts = rest.trim_end_matches('/').split('/');
    let owner = parts.next()?;
    let name = parts.next()?.trim_end_matches(".git");
    let repository = format!("{owner}/{name}");
    super::sources::repository_valid(&repository).then_some(repository)
}

fn full_sha(value: Option<&str>) -> Option<String> {
    value
        .filter(|sha| sha.len() == 40 && sha.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .map(str::to_owned)
}

fn index_entries(
    index: &Value,
    format: IndexFormat,
    source: &ExtensionSource,
    revision: &str,
    tree: &Tree,
) -> Result<Vec<Pending>> {
    let plugins = index["plugins"]
        .as_array()
        .context("Marketplace index has no plugins list")?;
    let mut entries = Vec::new();
    for plugin in plugins {
        let Some(name) = text(plugin, "name") else {
            continue;
        };
        let interface = plugin.get("interface").unwrap_or(&Value::Null);
        let mut entry = DirectoryEntry {
            id: format!("{}#{name}", source.repository),
            display_name: text(plugin, "displayName")
                .or_else(|| text(interface, "displayName"))
                .unwrap_or_else(|| name.clone()),
            description: text(plugin, "description")
                .or_else(|| text(interface, "shortDescription"))
                .unwrap_or_default(),
            version: text(plugin, "version"),
            author: person(plugin.get("author")).or_else(|| text(interface, "developerName")),
            homepage: text(plugin, "homepage").or_else(|| text(interface, "websiteURL")),
            categories: strings(plugin.get("category")),
            keywords: strings(plugin.get("keywords"))
                .into_iter()
                .chain(strings(plugin.get("tags")))
                .collect(),
            components: Vec::new(),
            source: source.name.clone(),
            install: None,
            unavailable: None,
            installed: false,
            update_available: false,
            name,
        };
        if matches!(format, IndexFormat::Codex)
            && plugin
                .pointer("/policy/installation")
                .and_then(Value::as_str)
                .is_some_and(|policy| policy != "AVAILABLE")
        {
            entry.unavailable = Some("The publisher has not made this plugin available".into());
        }
        let mut manifest = None;
        match plugin.get("source") {
            // A path inside this repository.
            Some(Value::String(path)) => {
                local(&mut entry, &mut manifest, source, revision, tree, path)
            }
            Some(Value::Object(spec)) => match spec.get("source").and_then(Value::as_str) {
                Some("local") => {
                    let path = spec.get("path").and_then(Value::as_str).unwrap_or_default();
                    local(&mut entry, &mut manifest, source, revision, tree, path);
                }
                Some(kind @ ("url" | "github" | "git-subdir")) => {
                    let repository = match kind {
                        "github" => spec
                            .get("repo")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                            .filter(|repo| super::sources::repository_valid(repo)),
                        _ => spec
                            .get("url")
                            .and_then(Value::as_str)
                            .and_then(github_repository),
                    };
                    let pinned = full_sha(spec.get("sha").and_then(Value::as_str));
                    let path = spec
                        .get("path")
                        .and_then(Value::as_str)
                        .map(|path| path.trim_matches('/').to_owned())
                        .filter(|path| !path.is_empty())
                        .unwrap_or_else(|| ".".into());
                    match (repository, pinned) {
                        (Some(repository), Some(sha)) if entry.unavailable.is_none() => {
                            let reference = spec
                                .get("ref")
                                .and_then(Value::as_str)
                                .unwrap_or(&sha)
                                .to_owned();
                            entry.install = Some(DiscoveredExtension {
                                source: ExtensionSource {
                                    name: source.name.clone(),
                                    repository,
                                    revision: reference,
                                    subdirectory: String::new(),
                                },
                                path,
                                name: entry.name.clone(),
                                revision: sha.clone(),
                                content: Some(sha),
                            });
                        }
                        (None, _) => {
                            entry.unavailable.get_or_insert_with(|| {
                                "Only GitHub-hosted plugins can be installed".into()
                            });
                        }
                        (Some(_), None) => {
                            entry.unavailable.get_or_insert_with(|| {
                                "The index does not pin this plugin to a commit".into()
                            });
                        }
                        _ => {}
                    }
                }
                _ => {
                    entry.unavailable = Some("Unsupported plugin source".into());
                }
            },
            _ => entry.unavailable = Some("The index entry has no source".into()),
        }
        for (key, kind) in [
            ("skills", ComponentKind::Skill),
            ("commands", ComponentKind::Command),
            ("agents", ComponentKind::Agent),
            ("hooks", ComponentKind::Hooks),
            ("mcpServers", ComponentKind::Mcp),
            ("lspServers", ComponentKind::LanguageServer),
            ("apps", ComponentKind::HostedIntegration),
        ] {
            if plugin.get(key).is_some() && !entry.components.contains(&kind) {
                entry.components.push(kind);
            }
        }
        entries.push((entry, manifest));
    }
    Ok(entries)
}

/// Fill an entry whose package lives inside the indexed repository.
fn local(
    entry: &mut DirectoryEntry,
    manifest: &mut Option<(String, String)>,
    source: &ExtensionSource,
    revision: &str,
    tree: &Tree,
    path: &str,
) {
    let root = path.trim_start_matches("./").trim_matches('/');
    let root = if root.is_empty() { "." } else { root };
    let valid = root == "."
        || root
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..");
    if !valid {
        entry.unavailable = Some("The index points outside the repository".into());
        return;
    }
    entry.components = tree.components(root);
    *manifest = tree.manifest(root);
    let content = if root == "." {
        Some(revision.to_owned())
    } else {
        tree.trees.get(root).cloned()
    };
    if content.is_none() {
        entry.unavailable = Some("The index lists a folder that is not in the repository".into());
        return;
    }
    if entry.unavailable.is_none() {
        entry.install = Some(DiscoveredExtension {
            source: source.clone(),
            path: root.to_owned(),
            name: entry.name.clone(),
            revision: revision.to_owned(),
            content,
        });
    }
}

/// Repositories without an index: one entry per package found in the tree.
fn tree_entries(source: &ExtensionSource, revision: &str, tree: &Tree) -> Result<Vec<Pending>> {
    let listing = Value::Array(
        tree.blobs
            .keys()
            .map(|path| serde_json::json!({ "path": path }))
            .collect(),
    );
    let found = super::sources::packages_from_tree(
        &serde_json::json!({"truncated": false, "tree": listing}),
        source,
        revision,
    )?;
    Ok(found
        .into_iter()
        .map(|mut item| {
            item.content = if item.path == "." {
                Some(revision.to_owned())
            } else {
                tree.trees.get(&item.path).cloned()
            };
            let entry = DirectoryEntry {
                id: format!("{}#{}", source.repository, item.path),
                name: item.name.clone(),
                display_name: item.name.clone(),
                description: String::new(),
                version: None,
                author: None,
                homepage: None,
                categories: Vec::new(),
                keywords: Vec::new(),
                components: tree.components(&item.path),
                source: source.name.clone(),
                unavailable: None,
                installed: false,
                update_available: false,
                install: Some(item.clone()),
            };
            (entry, tree.manifest(&item.path))
        })
        .collect())
}

fn needs_manifest(entry: &DirectoryEntry) -> bool {
    entry.description.is_empty()
        || entry.version.is_none()
        || entry.author.is_none()
        || entry.homepage.is_none()
        || entry.display_name == entry.name
}

fn manifest_details(path: &str, bytes: &[u8]) -> Result<ManifestDetails> {
    if path.ends_with("SKILL.md") {
        let markdown = std::str::from_utf8(bytes)?.replace("\r\n", "\n");
        let Some(rest) = markdown.strip_prefix("---\n") else {
            return Ok(ManifestDetails::default());
        };
        let (yaml, _) = rest
            .split_once("\n---")
            .context("Missing frontmatter closing delimiter")?;
        let meta: Value = super::manifest::parse_yaml(yaml)?;
        return Ok(ManifestDetails {
            display_name: text(&meta, "name"),
            description: text(&meta, "description"),
            version: text(&meta, "version").or_else(|| {
                meta.pointer("/metadata/version")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            }),
            ..Default::default()
        });
    }
    let manifest: Value = serde_json::from_slice(bytes)?;
    let interface = manifest.get("interface").unwrap_or(&Value::Null);
    if !manifest.is_object() {
        bail!("Plugin manifest is not an object");
    }
    Ok(ManifestDetails {
        display_name: text(interface, "displayName").or_else(|| text(&manifest, "displayName")),
        description: text(interface, "shortDescription").or_else(|| text(&manifest, "description")),
        version: text(&manifest, "version"),
        author: person(manifest.get("author")).or_else(|| text(interface, "developerName")),
        homepage: text(&manifest, "homepage").or_else(|| text(interface, "websiteURL")),
        categories: strings(interface.get("category")),
        keywords: strings(manifest.get("keywords")),
    })
}

fn fill(entry: &mut DirectoryEntry, details: &ManifestDetails) {
    if entry.display_name == entry.name
        && let Some(name) = &details.display_name
    {
        entry.display_name = name.clone();
    }
    if entry.description.is_empty()
        && let Some(description) = &details.description
    {
        entry.description = description.clone();
    }
    if entry.version.is_none() {
        entry.version = details.version.clone();
    }
    if entry.author.is_none() {
        entry.author = details.author.clone();
    }
    if entry.homepage.is_none() {
        entry.homepage = details.homepage.clone();
    }
    if entry.categories.is_empty() {
        entry.categories = details.categories.clone();
    }
    if entry.keywords.is_empty() {
        entry.keywords = details.keywords.clone();
    }
}

#[cfg(test)]
#[path = "marketplace/tests.rs"]
mod tests;
