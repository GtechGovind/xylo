//! Managed OS sandbox launcher. Missing dependencies fail closed; the caller
//! owns the returned process group and cancellation for its whole lifetime.
use anyhow::{Context, Result, ensure};
use serde_json::json;
use std::path::{Path, PathBuf};

pub struct PreparedSandbox {
    pub command: tokio::process::Command,
    // Keep the private temporary directory alive until descendants terminate.
    pub temporary: tempfile::TempDir,
}
pub fn prepare(
    program: &str,
    args: &[String],
    workspace: &Path,
    read_roots: &[PathBuf],
    protected_paths: &[PathBuf],
) -> Result<PreparedSandbox> {
    prepare_with_policy(program, args, workspace, read_roots, protected_paths, &[])
}
pub fn prepare_with_policy(
    program: &str,
    args: &[String],
    workspace: &Path,
    read_roots: &[PathBuf],
    protected_paths: &[PathBuf],
    allowed_domains: &[String],
) -> Result<PreparedSandbox> {
    prepare_internal(
        program,
        args,
        workspace,
        read_roots,
        protected_paths,
        allowed_domains,
        false,
    )
}
/// Coding commands may use installed host Rust binaries, but never the user's
/// Cargo config/credentials. Cache/home writes stay in an owned temporary root.
pub fn prepare_coding(
    program: &str,
    args: &[String],
    workspace: &Path,
    read_roots: &[PathBuf],
    protected_paths: &[PathBuf],
) -> Result<PreparedSandbox> {
    prepare_internal(
        program,
        args,
        workspace,
        read_roots,
        protected_paths,
        &[],
        true,
    )
}
fn prepare_internal(
    program: &str,
    args: &[String],
    workspace: &Path,
    read_roots: &[PathBuf],
    protected_paths: &[PathBuf],
    allowed_domains: &[String],
    coding: bool,
) -> Result<PreparedSandbox> {
    ensure!(
        cfg!(any(target_os = "macos", target_os = "linux")),
        "Managed sandbox is not qualified on this platform"
    );
    let workspace = workspace
        .canonicalize()
        .context("Sandbox workspace unavailable")?;
    let helper = helper_path()?;
    let node = runtime_node()?;
    ensure!(
        !node.starts_with(&workspace)
            && !read_roots
                .iter()
                .any(|root| root.canonicalize().is_ok_and(|root| node.starts_with(root))),
        "Sandbox launcher must be installed outside the mutable project and plugin packages"
    );
    let temporary = tempfile::tempdir()?;
    let (toolchain_read_roots, coding_environment) = if coding {
        rust_toolchain_scope(temporary.path(), &workspace, protected_paths, program, args)?
    } else {
        (Vec::new(), std::collections::BTreeMap::new())
    };
    let spec = json!({"program":program,"args":args,"workspace":workspace,"read_roots":read_roots,"protected_paths":protected_paths,"trusted_paths":trusted_paths_with_node(&node)?,"toolchain_read_roots":toolchain_read_roots,"coding_environment":coding_environment,"temporary":temporary.path(),"allowed_domains":allowed_domains});
    let mut command = tokio::process::Command::new(&node);
    command
        .arg(helper)
        .arg(serde_json::to_string(&spec)?)
        .current_dir(&workspace)
        .env_clear()
        .kill_on_drop(true);
    let safe_path = launcher_path(std::env::var_os("PATH").as_deref(), &workspace, read_roots)?;
    let mut directories = vec![
        node.parent()
            .context("Node runtime directory missing")?
            .to_path_buf(),
    ];
    directories.extend(std::env::split_paths(&safe_path));
    command.env("PATH", std::env::join_paths(directories)?);
    // Runtime lookup variables only. API keys, token caches and arbitrary parent
    // environment variables are never passed to plugin children.
    for name in [
        "HOME",
        "USER",
        "LOGNAME",
        "LANG",
        "LC_ALL",
        "TMPDIR",
        "SYSTEMROOT",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    Ok(PreparedSandbox { command, temporary })
}
pub fn helper_path() -> Result<PathBuf> {
    let executable = std::env::current_exe()?;
    let packaged = executable
        .parent()
        .context("Missing executable directory")?
        .join("../Resources/sandbox-helper/run.mjs");
    let development = Path::new(env!("CARGO_MANIFEST_DIR")).join("sandbox-helper/run.mjs");
    let helper = if packaged.is_file() {
        packaged
    } else {
        development
    };
    ensure!(helper.is_file(), "Managed sandbox helper is missing");
    Ok(helper.canonicalize()?)
}
fn rust_toolchain_scope(
    temporary: &Path,
    workspace: &Path,
    protected_paths: &[PathBuf],
    program: &str,
    args: &[String],
) -> Result<(Vec<PathBuf>, std::collections::BTreeMap<String, String>)> {
    use std::collections::BTreeMap;
    let mut roots = Vec::new();
    let mut environment = BTreeMap::new();
    let cargo_home = temporary.join("cargo-home");
    std::fs::create_dir(&cargo_home)?;
    environment.insert(
        "CARGO_HOME".into(),
        cargo_home.to_string_lossy().into_owned(),
    );
    environment.insert("CARGO_NET_OFFLINE".into(), "true".into());
    if let Some(home) = std::env::var_os("HOME")
        .map(PathBuf::from)
        .and_then(|path| path.canonicalize().ok())
    {
        let cargo = std::env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".cargo"));
        let rustup = std::env::var_os("RUSTUP_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".rustup"));
        for (path, base) in [
            (cargo.join("bin"), &cargo),
            (rustup.join("toolchains"), &rustup),
            (rustup.join("settings.toml"), &rustup),
        ] {
            if let Ok(path) = path.canonicalize() {
                let base = base.canonicalize()?;
                ensure!(
                    path.starts_with(&base) && path != base,
                    "Rust toolchain path escapes its configured toolchain home"
                );
                // Only fixed toolchain subpaths, never arbitrary executable
                // parents, whole HOME, Cargo registries, config or credentials.
                ensure!(
                    path != home && !home.starts_with(&path) && !path.starts_with(workspace),
                    "Rust toolchain grant overlaps HOME or mutable project root"
                );
                ensure!(
                    !protected_paths
                        .iter()
                        .filter_map(|path| path.canonicalize().ok())
                        .any(|protected| path.starts_with(&protected)
                            || protected.starts_with(&path)),
                    "Rust toolchain grant overlaps protected application data"
                );
                roots.push(path);
            }
        }
        if rustup.join("toolchains").is_dir() {
            environment.insert(
                "RUSTUP_HOME".into(),
                rustup.canonicalize()?.to_string_lossy().into_owned(),
            );
        }
        if workspace.join("Cargo.toml").is_file()
            || program.contains("cargo")
            || args.iter().any(|arg| arg.contains("cargo"))
        {
            public_cargo_cache_view(&cargo, &cargo_home, &mut roots)?;
        }
        for path in &roots {
            ensure!(
                path != &home && !home.starts_with(path) && !path.starts_with(workspace),
                "Coding read grant overlaps HOME or mutable project root"
            );
            ensure!(
                !protected_paths
                    .iter()
                    .filter_map(|path| path.canonicalize().ok())
                    .any(|protected| path.starts_with(&protected) || protected.starts_with(path)),
                "Coding read grant overlaps protected application data"
            );
        }
        if cargo.join("bin").is_dir() {
            let bin = cargo.join("bin").canonicalize()?;
            let mut path = vec![bin];
            path.extend(std::env::split_paths(&launcher_path(
                std::env::var_os("PATH").as_deref(),
                workspace,
                &[],
            )?));
            environment.insert(
                "PATH".into(),
                std::env::join_paths(path)?.to_string_lossy().into_owned(),
            );
        }
    }
    Ok((roots, environment))
}

/// An isolated Cargo layout may read the standard public sparse cache. Config,
/// credentials, git checkouts and custom/private registries are never mounted.
fn public_cargo_cache_view(
    host_cargo: &Path,
    isolated: &Path,
    roots: &mut Vec<PathBuf>,
) -> Result<()> {
    let host_cargo = host_cargo.canonicalize()?;
    let registry = host_cargo.join("registry");
    let Ok(registry) = registry.canonicalize() else {
        return Ok(());
    };
    ensure!(
        registry.starts_with(&host_cargo) && registry != host_cargo,
        "Cargo registry escapes its configured Cargo home"
    );
    let Ok(indexes) = std::fs::read_dir(registry.join("index")) else {
        return Ok(());
    };
    let mut count = 0;
    for entry in indexes {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with("index.crates.io-")
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
        {
            continue;
        }
        ensure!(count < 8, "Too many public Cargo registry indexes");
        count += 1;
        if entry.file_type()?.is_symlink() {
            continue;
        }
        let index = entry.path().canonicalize()?;
        ensure!(
            index.starts_with(&registry),
            "Cargo registry index escapes its cache"
        );
        use std::io::Read;
        let mut config = Vec::new();
        std::fs::File::open(index.join("config.json"))?
            .take(8193)
            .read_to_end(&mut config)?;
        ensure!(
            config.len() <= 8192,
            "Public registry configuration exceeds 8 KiB"
        );
        let config: serde_json::Value = serde_json::from_slice(&config)?;
        if config.get("dl").and_then(serde_json::Value::as_str)
            != Some("https://static.crates.io/crates")
            || config
                .get("api")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|api| api != "https://crates.io")
            || config
                .get("auth-required")
                .and_then(serde_json::Value::as_bool)
                == Some(true)
        {
            continue;
        }
        for kind in ["index", "cache", "src"] {
            let host = registry.join(kind).join(name);
            if std::fs::symlink_metadata(&host)
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
            {
                continue;
            }
            let Ok(host) = host.canonicalize() else {
                continue;
            };
            ensure!(
                host.starts_with(&registry),
                "Cargo cache grant escapes registry root"
            );
            let target = isolated.join("registry").join(kind).join(name);
            std::fs::create_dir_all(target.parent().unwrap())?;
            #[cfg(unix)]
            if kind == "index" {
                std::fs::create_dir(&target)?;
                std::fs::write(
                    target.join("config.json"),
                    br#"{"dl":"https://static.crates.io/crates","api":"https://crates.io"}"#,
                )?;
                if let Ok(cache) = host.join(".cache").canonicalize() {
                    ensure!(
                        cache.starts_with(&host),
                        "Sparse cache escapes its public index"
                    );
                    roots.push(cache.clone());
                    std::os::unix::fs::symlink(cache, target.join(".cache"))?;
                }
            } else if kind == "cache" {
                roots.push(host.clone());
                std::os::unix::fs::symlink(&host, &target)?;
            } else {
                roots.push(host.clone());
                // Writable isolated parent lets Cargo extract a cached archive
                // that is not already unpacked, without touching host sources.
                std::fs::create_dir(&target)?;
                let mut packages = 0;
                for package in std::fs::read_dir(&host)? {
                    let package = package?;
                    if !package.file_type()?.is_dir() {
                        continue;
                    }
                    ensure!(
                        packages < 20000,
                        "Public Cargo source cache exceeds entry budget"
                    );
                    packages += 1;
                    std::os::unix::fs::symlink(package.path(), target.join(package.file_name()))?;
                }
            }
        }
    }
    Ok(())
}

fn find_program(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .filter(|path| path.is_absolute())
        .map(|p| p.join(name))
        .find(|p| executable_file(p))
        .and_then(|path| path.canonicalize().ok())
}
fn executable_file(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}
fn launcher_path(
    value: Option<&std::ffi::OsStr>,
    workspace: &Path,
    read_roots: &[PathBuf],
) -> Result<std::ffi::OsString> {
    let roots = read_roots
        .iter()
        .map(|root| root.canonicalize())
        .collect::<std::io::Result<Vec<_>>>()?;
    let directories =
        std::env::split_paths(value.unwrap_or_else(|| std::ffi::OsStr::new("/usr/bin:/bin")))
            .filter(|path| path.is_absolute())
            .filter_map(|path| path.canonicalize().ok())
            .filter(|path| {
                path.is_dir()
                    && !path.starts_with(workspace)
                    && !roots.iter().any(|root| path.starts_with(root))
            })
            .collect::<Vec<_>>();
    ensure!(
        !directories.is_empty(),
        "No trusted system helper paths outside the project"
    );
    Ok(std::env::join_paths(directories)?)
}
pub fn runtime_node() -> Result<PathBuf> {
    let packaged = helper_path()?
        .parent()
        .context("Missing sandbox helper directory")?
        .join("runtime/bin/node");
    if executable_file(&packaged) {
        return Ok(packaged.canonicalize()?);
    }
    if let Some(node) = find_program("node") {
        return Ok(node);
    }
    // Finder-launched macOS applications do not inherit the user's shell PATH.
    #[cfg(target_os = "macos")]
    for installed in ["/opt/homebrew/bin/node", "/usr/local/bin/node"] {
        let path = Path::new(installed);
        if executable_file(path) {
            return Ok(path.canonicalize()?);
        }
    }
    anyhow::bail!("Node.js 22.12 or later is required for sandboxed tools")
}

/// Host binaries and sandbox implementation cannot be rewritten by a tool,
/// including when developing Xylo itself inside the selected workspace.
pub fn trusted_paths() -> Result<Vec<PathBuf>> {
    trusted_paths_with_node(&runtime_node()?)
}
fn trusted_paths_with_node(node: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = vec![
        helper_path()?
            .parent()
            .context("Sandbox helper directory missing")?
            .to_path_buf(),
    ];
    let executable = std::env::current_exe()?.canonicalize()?;
    paths.push(
        executable
            .parent()
            .context("Host executable directory missing")?
            .to_path_buf(),
    );
    paths.push(node.to_owned());
    Ok(paths)
}
pub fn ensure_writable_target(path: &Path) -> Result<()> {
    for protected in trusted_paths()? {
        ensure!(
            !path.starts_with(protected),
            "Tool cannot modify the application execution boundary"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn launcher_path_excludes_relative_project_and_package_directories() {
        let project = tempfile::tempdir().unwrap();
        let package = tempfile::tempdir().unwrap();
        let system = tempfile::tempdir().unwrap();
        let project = project.path().canonicalize().unwrap();
        let package = package.path().canonicalize().unwrap();
        let system = system.path().canonicalize().unwrap();
        let value = std::env::join_paths([
            Path::new("."),
            project.as_path(),
            package.as_path(),
            system.as_path(),
        ])
        .unwrap();
        let actual = launcher_path(Some(&value), &project, &[package]).unwrap();
        assert_eq!(
            std::env::split_paths(&actual).collect::<Vec<_>>(),
            vec![system]
        );
    }
    #[test]
    fn launcher_cannot_live_in_the_mutable_project() {
        let node = runtime_node().unwrap();
        let result = prepare(
            "/bin/echo",
            &["not executed".into()],
            node.parent().unwrap(),
            &[],
            &[],
        );
        assert!(
            result
                .err()
                .unwrap()
                .to_string()
                .contains("outside the mutable project")
        );
    }
    #[test]
    fn sandbox_does_not_inherit_provider_environment() {
        let dir = tempfile::tempdir().unwrap();
        let prepared = prepare("/bin/echo", &["hello".into()], dir.path(), &[], &[]).unwrap();
        let vars: Vec<_> = prepared
            .command
            .as_std()
            .get_envs()
            .filter_map(|(key, value)| value.map(|_| key.to_string_lossy().into_owned()))
            .collect();
        assert!(vars.iter().all(|k| {
            [
                "PATH",
                "HOME",
                "USER",
                "LOGNAME",
                "LANG",
                "LC_ALL",
                "TMPDIR",
                "SYSTEMROOT",
            ]
            .contains(&k.as_str())
        }));
    }
}

/// Plugin-controlled environment reaches only the sandboxed child. It must never
/// influence the trusted Node launcher, runtime import, or policy initialization.
pub fn set_child_environment(
    prepared: &mut PreparedSandbox,
    environment: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    ensure!(
        environment.len() <= 128,
        "Too many child environment variables"
    );
    for (name, value) in environment {
        ensure!(
            !name.is_empty()
                && name.len() <= 128
                && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                && value.len() <= 65536
                && !value.contains('\0'),
            "Invalid child environment variable"
        );
        ensure!(
            name != "XYLO_SANDBOX_CHILD_ENV",
            "Reserved child environment variable"
        );
    }
    let serialized = serde_json::to_string(environment)?;
    ensure!(
        serialized.len() <= 256 * 1024,
        "Child environment exceeds 256 KiB"
    );
    prepared.command.env("XYLO_SANDBOX_CHILD_ENV", serialized);
    Ok(())
}

#[cfg(test)]
mod coding_scope_tests {
    use super::*;
    #[test]
    fn public_cache_view_excludes_private_registry_config_and_credentials() {
        let directory = tempfile::tempdir().unwrap();
        let cargo = directory.path().join("host-cargo");
        let isolated = directory.path().join("isolated");
        std::fs::create_dir_all(&isolated).unwrap();
        let name = "index.crates.io-fixture";
        for kind in ["index", "cache", "src"] {
            std::fs::create_dir_all(cargo.join("registry").join(kind).join(name)).unwrap();
        }
        std::fs::create_dir(cargo.join("registry/index").join(name).join(".cache")).unwrap();
        std::fs::write(cargo.join("credentials.toml"), "private-token").unwrap();
        std::fs::write(cargo.join("config.toml"), "private-config").unwrap();
        let config = cargo.join("registry/index").join(name).join("config.json");
        std::fs::write(
            &config,
            r#"{"dl":"https://private.example","auth-required":true}"#,
        )
        .unwrap();
        let mut roots = Vec::new();
        public_cargo_cache_view(&cargo, &isolated, &mut roots).unwrap();
        assert!(roots.is_empty());
        std::fs::write(
            &config,
            r#"{"dl":"https://static.crates.io/crates","api":"https://crates.io","unused-token":"not-for-child"}"#,
        )
        .unwrap();
        public_cargo_cache_view(&cargo, &isolated, &mut roots).unwrap();
        assert_eq!(roots.len(), 3);
        assert!(!isolated.join("credentials.toml").exists());
        assert!(!isolated.join("config.toml").exists());
        assert!(
            !std::fs::read_to_string(
                isolated
                    .join("registry/index")
                    .join(name)
                    .join("config.json")
            )
            .unwrap()
            .contains("not-for-child")
        );
        assert!(
            roots
                .iter()
                .all(|path| path.starts_with(cargo.join("registry").canonicalize().unwrap()))
        );
        #[cfg(unix)]
        assert!(
            std::fs::symlink_metadata(isolated.join("registry/index").join(name).join(".cache"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
    #[test]
    fn plugin_scope_has_no_implicit_cargo_home_or_toolchain_grants() {
        let directory = tempfile::tempdir().unwrap();
        let prepared = prepare("/bin/echo", &["scope".into()], directory.path(), &[], &[]).unwrap();
        let spec = prepared
            .command
            .as_std()
            .get_args()
            .nth(1)
            .unwrap()
            .to_str()
            .unwrap();
        let spec: serde_json::Value = serde_json::from_str(spec).unwrap();
        assert!(spec["toolchain_read_roots"].as_array().unwrap().is_empty());
        assert!(spec["coding_environment"].as_object().unwrap().is_empty());
    }
}
