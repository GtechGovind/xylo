use super::*;

fn workspace() -> (&'static str, &'static str) {
    if cfg!(windows) {
        (
            r"C:\Users\private-person\project",
            r"C:\Users\private-person\project\src\main.rs",
        )
    } else {
        (
            "/Users/private-person/project",
            "/Users/private-person/project/src/main.rs",
        )
    }
}

#[test]
fn native_workspace_paths_retain_only_safe_relative_evidence() {
    let (scope, path) = workspace();
    assert_eq!(relative_path(path, scope).as_deref(), Some("src/main.rs"));
    assert_eq!(
        relative_path("src/./main.rs", scope).as_deref(),
        Some("src/main.rs")
    );
    assert_eq!(relative_path(scope, scope).as_deref(), Some("."));
    let retained = arguments(&json!({"path":path,"api_key":"should-not-survive"}), scope);
    assert_eq!(retained["path"], "src/main.rs");
    assert_eq!(retained["api_key"], json!({"$input":"redacted"}));
    let encoded = retained.to_string();
    assert!(!encoded.contains("private-person"));
    assert!(!encoded.contains("should-not-survive"));
}

#[test]
fn relative_evidence_refuses_external_hidden_traversal_and_auth_paths() {
    let (scope, _) = workspace();
    let outside = format!("{scope}-other/src/main.rs");
    for path in [
        outside.as_str(),
        "../other/main.rs",
        "src/../main.rs",
        "src/.env",
        "src/.private/main.rs",
        "src/access_token.txt",
        "src/main.rs?api_key=should-not-survive",
        "https://example.invalid/private",
        "src\0/main.rs",
        r"src\..\..\private",
    ] {
        assert_eq!(
            arguments(&json!({"path":path}), scope),
            json!({"path":{"$input":"path"}}),
            "Unsafe evidence was retained for {path:?}"
        );
    }
    assert!(relative_path(&"x".repeat(257), scope).is_none());
}

#[cfg(windows)]
#[test]
fn canonical_windows_namespaces_require_an_exact_workspace_prefix() {
    let scope = r"\\?\C:\Users\private-person\project";
    assert_eq!(
        relative_path(r"\\?\C:\Users\private-person\project\src\main.rs", scope).as_deref(),
        Some("src/main.rs")
    );
    assert_eq!(
        relative_path(r"src\main.rs", scope).as_deref(),
        Some("src/main.rs")
    );
    for path in [
        r"C:src\main.rs",
        r"\src\main.rs",
        r"\\?\C:\Users\private-person\project-other\src\main.rs",
        r"\\?\D:\Users\private-person\project\src\main.rs",
        r"C:\Users\private-person\project\src\main.rs",
    ] {
        assert!(relative_path(path, scope).is_none(), "Accepted {path:?}");
    }
}
