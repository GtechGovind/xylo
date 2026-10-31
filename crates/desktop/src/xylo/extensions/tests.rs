use super::{
    ComponentKind, DirectorySourceStatus, ExtensionPackage, ExtensionSource, built_in_source,
    default_sources, display_name, freshness_caption, installed_package, review_groups,
};
use serde_json::{Value, json};

fn component(name: &str, kind: ComponentKind) -> Value {
    json!({
        "name": name,
        "kind": kind,
        "path": name,
        "description": "",
        "readiness": "installed",
        "requirements": [],
    })
}
fn status(fetched_at: Option<i64>) -> DirectorySourceStatus {
    serde_json::from_value(json!({
        "source": default_sources()[0],
        "freshness": if fetched_at.is_some() { "fresh" } else { "missing" },
        "fetched_at": fetched_at,
        "revision": null,
        "entries": 0,
        "message": null,
    }))
    .unwrap()
}

#[test]
fn review_lists_executable_components_first_and_skips_empty_groups() {
    let package: ExtensionPackage = serde_json::from_value(json!({
        "id": "p",
        "name": "Package",
        "version": "1",
        "source": "local",
        "revision": null,
        "digest": "d",
        "enabled": false,
        "components": [
            component("guide", ComponentKind::Skill),
            component("hooks", ComponentKind::Hooks),
            component("server", ComponentKind::Mcp),
            component("other-guide", ComponentKind::Skill),
        ],
    }))
    .unwrap();
    let groups = review_groups(&package);
    let kinds: Vec<_> = groups.iter().map(|(kind, _)| kind.clone()).collect();
    assert_eq!(
        kinds,
        [
            ComponentKind::Hooks,
            ComponentKind::Mcp,
            ComponentKind::Skill
        ]
    );
    assert_eq!(groups[2].1, [0, 3]);
}

#[test]
fn installed_package_ids_read_as_names() {
    assert_eq!(display_name("build-ios-apps"), "Build iOS apps");
    assert_eq!(display_name("github_mcp-server"), "GitHub MCP server");
    assert_eq!(display_name("pdf"), "PDF");
    assert_eq!(display_name("OpenAI-docs"), "OpenAI docs");
    assert_eq!(display_name("--"), "--");
}

#[test]
fn only_custom_sources_are_removable() {
    assert!(default_sources().iter().all(built_in_source));
    let custom = ExtensionSource::from_input("someone/plugins").unwrap();
    assert!(!built_in_source(&custom));
}

#[test]
fn freshness_names_the_oldest_shown_source() {
    let now = xylo_core::now_ms() / 1000;
    let fresh = status(Some(now - 30));
    let older = status(Some(now - 600));
    let missing = status(None);
    assert_eq!(
        freshness_caption(&[&fresh, &older], false),
        "Updated 10 min ago"
    );
    assert_eq!(freshness_caption(&[&missing], false), "Not loaded yet");
    assert_eq!(
        freshness_caption(&[&fresh], true),
        "Checking sources for updates…"
    );
}

fn package(id: &str, digest: &str) -> ExtensionPackage {
    serde_json::from_value(json!({
        "id": id,
        "name": id,
        "version": "1",
        "source": "local",
        "revision": null,
        "digest": digest,
        "enabled": false,
        "components": [],
    }))
    .unwrap()
}

#[test]
fn a_local_install_reviews_the_new_or_changed_package() {
    let before = [package("a", "1"), package("b", "1")];
    let added = [package("a", "1"), package("b", "1"), package("c", "1")];
    assert_eq!(
        installed_package(&before, &added).map(|p| p.id.as_str()),
        Some("c")
    );
    let replaced = [package("a", "1"), package("b", "2")];
    assert_eq!(
        installed_package(&before, &replaced).map(|p| p.id.as_str()),
        Some("b")
    );
    assert!(installed_package(&before, &before).is_none());
}
