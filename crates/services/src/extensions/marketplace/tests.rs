use super::*;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const CLAUDE: &str = "fixture/claude";
const CODEX: &str = "fixture/codex";
const EXTERNAL_SHA: &str = "1111111111111111111111111111111111111111";

#[derive(Default)]
struct Server {
    /// Current commit per repository.
    commits: BTreeMap<String, String>,
    /// Tree JSON per `repo@sha`.
    trees: BTreeMap<String, Value>,
    /// Raw files per `repo/sha/path`.
    files: BTreeMap<String, String>,
    rate_limited: bool,
    requests: Vec<String>,
}

type Shared = Arc<Mutex<Server>>;

async fn serve(state: Shared) -> DirectoryEndpoints {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let state = state.clone();
            tokio::spawn(async move {
                let (read, mut write) = stream.into_split();
                let mut reader = BufReader::new(read);
                let mut line = String::new();
                if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                    return;
                }
                let target = line.split_whitespace().nth(1).unwrap_or("").to_owned();
                let mut if_none_match = None;
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).await.unwrap_or(0) == 0 || header == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = header.split_once(':')
                        && name.eq_ignore_ascii_case("if-none-match")
                    {
                        if_none_match = Some(value.trim().to_owned());
                    }
                }
                let (status, headers, body) = respond(&state, &target, if_none_match);
                let response = format!(
                    "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = write.write_all(response.as_bytes()).await;
            });
        }
    });
    DirectoryEndpoints {
        api: format!("http://{address}/api"),
        raw: format!("http://{address}/raw"),
    }
}

fn respond(
    state: &Shared,
    target: &str,
    if_none_match: Option<String>,
) -> (String, String, String) {
    let mut server = state.lock().unwrap();
    server.requests.push(target.to_owned());
    let not_found = ("404 Not Found".to_owned(), String::new(), String::new());
    if let Some(api) = target.strip_prefix("/api/repos/") {
        if server.rate_limited {
            return (
                "403 Forbidden".into(),
                "x-ratelimit-remaining: 0\r\n".into(),
                "{\"message\":\"API rate limit exceeded\"}".into(),
            );
        }
        let path = api.split('?').next().unwrap_or(api);
        let parts: Vec<_> = path.split('/').collect();
        let repository = format!("{}/{}", parts[0], parts[1]);
        let Some(commit) = server.commits.get(&repository).cloned() else {
            return not_found;
        };
        if parts.get(2) == Some(&"commits") {
            let etag = format!("\"{commit}\"");
            if if_none_match.as_deref() == Some(etag.as_str()) {
                return (
                    "304 Not Modified".into(),
                    format!("ETag: {etag}\r\n"),
                    String::new(),
                );
            }
            return ("200 OK".into(), format!("ETag: {etag}\r\n"), commit);
        }
        if parts.get(2) == Some(&"git") {
            let sha = parts[4];
            return match server.trees.get(&format!("{repository}@{sha}")) {
                Some(tree) => ("200 OK".into(), String::new(), tree.to_string()),
                None => not_found,
            };
        }
        return not_found;
    }
    if let Some(raw) = target.strip_prefix("/raw/") {
        return match server.files.get(raw) {
            Some(body) => ("200 OK".into(), String::new(), body.clone()),
            None => not_found,
        };
    }
    not_found
}

fn tree(blobs: &[(&str, &str)], trees: &[(&str, &str)]) -> Value {
    let mut items: Vec<Value> = blobs
        .iter()
        .map(|(path, sha)| serde_json::json!({"path":path,"type":"blob","sha":sha}))
        .collect();
    items.extend(
        trees
            .iter()
            .map(|(path, sha)| serde_json::json!({"path":path,"type":"tree","sha":sha})),
    );
    serde_json::json!({"truncated":false,"tree":items})
}

/// Publish a Claude marketplace at `commit` whose reviewer folder has tree
/// hash `content`.
fn publish_claude(state: &Shared, commit: &str, content: &str) {
    let mut server = state.lock().unwrap();
    server.commits.insert(CLAUDE.into(), commit.into());
    server.trees.insert(
        format!("{CLAUDE}@{commit}"),
        tree(
            &[
                (".claude-plugin/marketplace.json", "index-blob"),
                (
                    "plugins/reviewer/.claude-plugin/plugin.json",
                    "reviewer-manifest",
                ),
                ("plugins/reviewer/skills/review/SKILL.md", "skill-blob"),
                ("plugins/reviewer/hooks/hooks.json", "hooks-blob"),
            ],
            &[("plugins/reviewer", content)],
        ),
    );
    server.files.insert(
        format!("{CLAUDE}/{commit}/.claude-plugin/marketplace.json"),
        serde_json::json!({"name":"fixture","plugins":[
            {"name":"reviewer","description":"Reviews   diffs\nbefore merge","author":{"name":"Acme"},"category":"development","source":"./plugins/reviewer","homepage":"https://acme.example","keywords":["review","quality"]},
            {"name":"external","description":"Language server","source":{"source":"git-subdir","url":"https://github.com/acme/tools.git","path":"plugins/lsp","ref":"v1","sha":EXTERNAL_SHA},"lspServers":{"rust":{}},"category":"development"},
            {"name":"unpinned","description":"Moves with main","source":{"source":"url","url":"https://github.com/acme/other.git"}},
            {"name":"elsewhere","description":"Not on GitHub","source":{"source":"url","url":"https://gitlab.example/acme/thing.git","sha":EXTERNAL_SHA}}
        ]})
        .to_string(),
    );
    server.files.insert(
        format!("{CLAUDE}/{commit}/plugins/reviewer/.claude-plugin/plugin.json"),
        r#"{"name":"reviewer","version":"1.2.0","description":"Manifest text the index already covers"}"#.into(),
    );
}

fn publish_codex(state: &Shared) {
    let commit = "c0dec0dec0dec0dec0dec0dec0dec0dec0dec0de";
    let mut server = state.lock().unwrap();
    server.commits.insert(CODEX.into(), commit.into());
    server.trees.insert(
        format!("{CODEX}@{commit}"),
        tree(
            &[
                (".agents/plugins/marketplace.json", "codex-index"),
                (
                    "plugins/linear/.codex-plugin/plugin.json",
                    "linear-manifest",
                ),
                ("plugins/linear/.mcp.json", "mcp-blob"),
            ],
            &[("plugins/linear", "linear-tree")],
        ),
    );
    server.files.insert(
        format!("{CODEX}/{commit}/.agents/plugins/marketplace.json"),
        serde_json::json!({"name":"curated","plugins":[
            {"name":"linear","source":{"source":"local","path":"./plugins/linear"},"policy":{"installation":"AVAILABLE"},"category":"Productivity"},
            {"name":"hidden","source":{"source":"local","path":"./plugins/linear"},"policy":{"installation":"NOT_AVAILABLE"},"category":"Productivity"}
        ]})
        .to_string(),
    );
    server.files.insert(
        format!("{CODEX}/{commit}/plugins/linear/.codex-plugin/plugin.json"),
        serde_json::json!({"name":"linear","version":"5.0.1","author":{"name":"Linear Orbit, Inc"},"homepage":"https://linear.app/","keywords":["issues"],
            "interface":{"displayName":"Linear","shortDescription":"Plan and build products","category":"Productivity"}})
        .to_string(),
    );
}

async fn fixture() -> (
    tempfile::TempDir,
    ExtensionService,
    Shared,
    DirectoryEndpoints,
) {
    let home = tempfile::tempdir().unwrap();
    let service = ExtensionService::new(home.path());
    service
        .save_sources(&[
            ExtensionSource {
                name: "Claude fixture".into(),
                repository: CLAUDE.into(),
                revision: "main".into(),
                subdirectory: String::new(),
            },
            ExtensionSource {
                name: "Codex fixture".into(),
                repository: CODEX.into(),
                revision: "main".into(),
                subdirectory: String::new(),
            },
        ])
        .unwrap();
    let state: Shared = Arc::default();
    publish_claude(&state, &"a".repeat(40), "reviewer-tree-1");
    publish_codex(&state);
    let endpoints = serve(state.clone()).await;
    (home, service, state, endpoints)
}

fn entry<'a>(page: &'a DirectoryPage, name: &str) -> &'a DirectoryEntry {
    page.entries
        .iter()
        .find(|entry| entry.name == name)
        .unwrap_or_else(|| panic!("{name} missing from {page:#?}"))
}

#[tokio::test]
async fn index_fields_win_and_manifests_fill_only_the_gaps() {
    let (_home, service, _state, endpoints) = fixture().await;
    assert!(
        service
            .directory(&DirectoryQuery::default())
            .unwrap()
            .sources
            .iter()
            .all(|status| status.freshness == DirectoryFreshness::Missing)
    );
    let statuses = service
        .refresh_directory_from(&endpoints, false)
        .await
        .unwrap();
    assert!(
        statuses
            .iter()
            .all(|status| status.freshness == DirectoryFreshness::Fresh),
        "{statuses:#?}"
    );
    let page = service.directory(&DirectoryQuery::default()).unwrap();
    let reviewer = entry(&page, "reviewer");
    assert_eq!(reviewer.description, "Reviews diffs before merge");
    assert_eq!(reviewer.version.as_deref(), Some("1.2.0"));
    assert_eq!(reviewer.author.as_deref(), Some("Acme"));
    assert_eq!(reviewer.homepage.as_deref(), Some("https://acme.example"));
    assert_eq!(reviewer.categories, vec!["development"]);
    assert_eq!(reviewer.keywords, vec!["review", "quality"]);
    assert_eq!(
        reviewer.components,
        vec![ComponentKind::Skill, ComponentKind::Hooks]
    );
    let install = reviewer.install.as_ref().unwrap();
    assert_eq!(install.path, "plugins/reviewer");
    assert_eq!(install.revision, "a".repeat(40));
    assert_eq!(install.content.as_deref(), Some("reviewer-tree-1"));

    let external = entry(&page, "external");
    let install = external.install.as_ref().unwrap();
    assert_eq!(install.source.repository, "acme/tools");
    assert_eq!(
        (install.path.as_str(), install.revision.as_str()),
        ("plugins/lsp", EXTERNAL_SHA)
    );
    assert_eq!(external.components, vec![ComponentKind::LanguageServer]);
    for name in ["unpinned", "elsewhere"] {
        let listed = entry(&page, name);
        assert!(
            listed.install.is_none() && listed.unavailable.is_some(),
            "{listed:#?}"
        );
    }

    let linear = entry(&page, "linear");
    assert_eq!(linear.display_name, "Linear");
    assert_eq!(linear.description, "Plan and build products");
    assert_eq!(linear.version.as_deref(), Some("5.0.1"));
    assert_eq!(linear.author.as_deref(), Some("Linear Orbit, Inc"));
    assert_eq!(linear.categories, vec!["Productivity"]);
    assert_eq!(linear.components, vec![ComponentKind::Mcp]);
    assert!(entry(&page, "hidden").install.is_none());
}

#[tokio::test]
async fn unchanged_sources_revalidate_with_a_304_and_reuse_the_cache() {
    let (_home, service, state, endpoints) = fixture().await;
    service
        .refresh_directory_from(&endpoints, false)
        .await
        .unwrap();
    let first = service.directory(&DirectoryQuery::default()).unwrap();
    state.lock().unwrap().requests.clear();
    // Within the TTL nothing is requested at all.
    service
        .refresh_directory_from(&endpoints, false)
        .await
        .unwrap();
    assert!(state.lock().unwrap().requests.is_empty());
    // A forced refresh revalidates; a 304 fetches no tree, index or manifest.
    let statuses = service
        .refresh_directory_from(&endpoints, true)
        .await
        .unwrap();
    let requests = state.lock().unwrap().requests.clone();
    assert_eq!(requests.len(), 2, "{requests:?}");
    assert!(requests.iter().all(|request| request.contains("/commits/")));
    assert!(
        statuses
            .iter()
            .all(|status| status.freshness == DirectoryFreshness::Fresh)
    );
    let second = service.directory(&DirectoryQuery::default()).unwrap();
    assert_eq!(first.entries, second.entries);
}

#[tokio::test]
async fn rate_limits_keep_cached_results_with_a_clear_status() {
    let (_home, service, state, endpoints) = fixture().await;
    service
        .refresh_directory_from(&endpoints, false)
        .await
        .unwrap();
    state.lock().unwrap().rate_limited = true;
    let statuses = service
        .refresh_directory_from(&endpoints, true)
        .await
        .unwrap();
    for status in &statuses {
        assert_eq!(status.freshness, DirectoryFreshness::RateLimited);
        assert_eq!(
            status.message.as_deref(),
            Some("Showing cached results; GitHub rate limit reached")
        );
    }
    let page = service.directory(&DirectoryQuery::default()).unwrap();
    assert_eq!(page.total, 6);
    assert!(page.sources.iter().all(|status| status.entries > 0));
    state.lock().unwrap().rate_limited = false;
    let statuses = service
        .refresh_directory_from(&endpoints, true)
        .await
        .unwrap();
    assert!(
        statuses
            .iter()
            .all(|status| status.freshness == DirectoryFreshness::Fresh)
    );
}

#[tokio::test]
async fn local_search_filters_and_pages_without_network() {
    let (_home, service, _state, endpoints) = fixture().await;
    service
        .refresh_directory_from(&endpoints, false)
        .await
        .unwrap();
    let search = |text: &str| {
        service
            .directory(&DirectoryQuery {
                text: text.into(),
                ..Default::default()
            })
            .unwrap()
    };
    assert_eq!(search("REVIEW").entries[0].name, "reviewer");
    assert_eq!(search("acme diffs").total, 1);
    let issues = search("issues");
    assert!(issues.entries.iter().any(|entry| entry.name == "linear"));
    assert!(
        issues
            .entries
            .iter()
            .all(|entry| entry.keywords == ["issues"])
    );
    assert_eq!(search("no such thing").total, 0);
    let category = service
        .directory(&DirectoryQuery {
            category: Some("productivity".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(category.total, 2);
    assert!(category.categories.contains(&"development".to_owned()));
    let one_source = service
        .directory(&DirectoryQuery {
            source: Some(CODEX.into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(one_source.sources.len(), 1);
    let all = service.directory(&DirectoryQuery::default()).unwrap();
    let page = service
        .directory(&DirectoryQuery {
            offset: 2,
            limit: 2,
            ..Default::default()
        })
        .unwrap();
    assert_eq!((page.total, page.offset, page.entries.len()), (6, 2, 2));
    assert_eq!(page.entries, all.entries[2..4]);
}

#[tokio::test]
async fn installed_and_update_flags_follow_the_installed_content() {
    let (_home, service, state, endpoints) = fixture().await;
    service
        .refresh_directory_from(&endpoints, false)
        .await
        .unwrap();
    let page = service.directory(&DirectoryQuery::default()).unwrap();
    let item = entry(&page, "reviewer").install.clone().unwrap();
    let package = tempfile::tempdir().unwrap();
    std::fs::write(
        package.path().join("SKILL.md"),
        "---\nname: reviewer\ndescription: test\n---\nBody",
    )
    .unwrap();
    service
        .install_snapshot(
            package.path(),
            &provenance(&item),
            Some(item.revision.clone()),
            "reviewer",
        )
        .unwrap();
    service.record_installed_content(&item).unwrap();
    let page = service.directory(&DirectoryQuery::default()).unwrap();
    let reviewer = entry(&page, "reviewer");
    assert!(reviewer.installed && !reviewer.update_available);
    assert!(!entry(&page, "linear").installed);
    let installed_only = service
        .directory(&DirectoryQuery {
            installed_only: true,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(installed_only.total, 1);

    // A new commit that leaves the reviewer folder unchanged is no update.
    publish_claude(&state, &"b".repeat(40), "reviewer-tree-1");
    service
        .refresh_directory_from(&endpoints, true)
        .await
        .unwrap();
    let page = service.directory(&DirectoryQuery::default()).unwrap();
    assert!(!entry(&page, "reviewer").update_available);
    // Changed folder content is.
    publish_claude(&state, &"c".repeat(40), "reviewer-tree-2");
    service
        .refresh_directory_from(&endpoints, true)
        .await
        .unwrap();
    let page = service.directory(&DirectoryQuery::default()).unwrap();
    let reviewer = entry(&page, "reviewer");
    assert!(reviewer.installed && reviewer.update_available);
}

#[test]
fn an_unreadable_directory_cache_is_deleted() {
    let home = tempfile::tempdir().unwrap();
    let service = ExtensionService::new(home.path());
    let source = default_sources().remove(0);
    let path = service.cache_path(&source);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"{\"old\":true}").unwrap();
    let page = service
        .directory(&DirectoryQuery {
            source: Some(source.repository.clone()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(page.sources[0].freshness, DirectoryFreshness::Missing);
    assert!(!path.exists());
}

#[test]
fn default_sources_are_the_two_plugin_marketplaces() {
    let repositories: Vec<_> = default_sources()
        .into_iter()
        .map(|source| source.repository)
        .collect();
    assert_eq!(
        repositories,
        vec!["openai/plugins", "anthropics/claude-plugins-official"]
    );
}

#[tokio::test]
#[ignore = "Reads public GitHub; run explicitly to qualify the real marketplace formats"]
async fn public_marketplaces_produce_rich_entries() {
    let home = tempfile::tempdir().unwrap();
    let service = ExtensionService::new(home.path());
    let statuses = service.refresh_directory(true).await.unwrap();
    for status in &statuses {
        assert_eq!(status.freshness, DirectoryFreshness::Fresh, "{status:#?}");
        assert!(status.entries > 10, "{status:#?}");
    }
    let page = service
        .directory(&DirectoryQuery {
            limit: 200,
            ..Default::default()
        })
        .unwrap();
    let described = page
        .entries
        .iter()
        .filter(|entry| !entry.description.is_empty())
        .count();
    assert!(described * 10 >= page.entries.len() * 9, "{page:#?}");
}
