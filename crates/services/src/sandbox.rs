//! Managed OS sandbox launcher. Missing dependencies fail closed; the caller
//! owns the returned process group and cancellation for its whole lifetime.
//!
//! The Node runtime that bootstraps the sandbox runs unsandboxed, so it must
//! be trusted: the bundled runtime next to the helper, then an explicitly
//! configured `XYLO_SANDBOX_NODE`, then a `node` discovered on `PATH` (or in
//! Homebrew on macOS). A discovered binary, including one under the home
//! directory (nvm, volta, fnm, asdf), is accepted only when it and every
//! directory above it, up to but not including `HOME` (or up to `/` outside
//! home), is owned by the current user or root and is not world-writable or
//! writable by a group other than a root-equivalent one (`wheel`, and `admin`
//! on macOS, which Homebrew uses). Every candidate must be at least Node
//! 22.12. When none is usable, the error is a [`SandboxUnavailable`] naming
//! each rejected binary, why, and how to fix it.
//!
//! Start failures and blocked connections are never reported on the
//! program's stdout or stderr, which the program controls. Each prepared
//! command has a private status file ([`PreparedSandbox::status_file`]) in its
//! temporary directory that the sandboxed program can neither read nor write;
//! only the launcher writes it. After the command exits, read it with
//! [`read_start_status`] (a [`StartFailure`] means nothing ran) and
//! [`read_network_denials`]. The file holds one JSON object per line:
//! `{"kind":"unavailable","reason":..}` (exit [`UNAVAILABLE_EXIT`]),
//! `{"kind":"spawn_failed","reason":..}` (exit [`SPAWN_FAILED_EXIT`]) or
//! `{"kind":"network_denied","host":"host:port"}`.
use anyhow::{Context, Result, ensure};
use serde_json::json;
use std::path::{Path, PathBuf};

/// The sandbox cannot start; nothing ran. Displays as an
/// [`UNAVAILABLE_MARKER`] line with the reason and how to fix it, so callers
/// can surface it directly or downcast it to classify the command as not
/// started.
#[derive(Debug)]
pub struct SandboxUnavailable(pub String);
impl std::fmt::Display for SandboxUnavailable {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(output, "{UNAVAILABLE_MARKER} {}", self.0)
    }
}
impl std::error::Error for SandboxUnavailable {}

/// Message prefix of [`SandboxUnavailable`] and [`StartFailure::Unavailable`].
/// It is text for people; nothing parses it.
pub const UNAVAILABLE_MARKER: &str = "XYLO_SANDBOX_UNAVAILABLE:";
/// Launcher exit status when the sandbox could not be set up.
pub const UNAVAILABLE_EXIT: i32 = 125;
/// Message prefix of [`StartFailure::SpawnFailed`]. Text for people only.
pub const SPAWN_FAILED_MARKER: &str = "XYLO_SANDBOX_SPAWN_FAILED:";
/// Launcher exit status when the program could not be started in the sandbox.
pub const SPAWN_FAILED_EXIT: i32 = 127;
/// Environment variable naming an explicitly trusted Node runtime. It takes
/// precedence over any discovered Node and is not subject to the ownership
/// checks applied to discovered binaries.
pub const NODE_OVERRIDE: &str = "XYLO_SANDBOX_NODE";
const MINIMUM_NODE: (u64, u64) = (22, 12);

/// A launcher command ready to spawn. Keep `temporary` alive until the
/// command and its descendants have exited and the status file was read.
pub struct PreparedSandbox {
    pub command: tokio::process::Command,
    /// Private root holding the child's `TMPDIR` and the status directory.
    pub temporary: tempfile::TempDir,
    /// The launcher's status file (see the module docs). It exists only once
    /// something was recorded.
    pub status_file: PathBuf,
}

/// Why a sandboxed command never started.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StartFailure {
    /// The sandbox could not be set up.
    Unavailable(String),
    /// The sandbox was set up but the program could not be started in it.
    SpawnFailed(String),
}
impl std::fmt::Display for StartFailure {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(reason) => write!(output, "{UNAVAILABLE_MARKER} {reason}"),
            Self::SpawnFailed(reason) => write!(output, "{SPAWN_FAILED_MARKER} {reason}"),
        }
    }
}

const MAX_STATUS_BYTES: u64 = 256 * 1024;

fn status_records(path: &Path) -> Vec<serde_json::Value> {
    use std::io::Read;
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let mut text = String::new();
    if file
        .take(MAX_STATUS_BYTES)
        .read_to_string(&mut text)
        .is_err()
    {
        return Vec::new();
    }
    text.lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// The recorded start failure, if the launcher recorded one. `None` means
/// the program may have run: its exit status and output are its own.
pub fn read_start_status(path: &Path) -> Option<StartFailure> {
    status_records(path).into_iter().find_map(|record| {
        let reason = record["reason"]
            .as_str()
            .unwrap_or("unknown reason")
            .to_owned();
        match record["kind"].as_str()? {
            "unavailable" => Some(StartFailure::Unavailable(reason)),
            "spawn_failed" => Some(StartFailure::SpawnFailed(reason)),
            _ => None,
        }
    })
}

/// Destinations (`host:port`) the sandbox refused under [`Network::None`],
/// in the order they were first attempted.
pub fn read_network_denials(path: &Path) -> Vec<String> {
    status_records(path)
        .into_iter()
        .filter(|record| record["kind"] == "network_denied")
        .filter_map(|record| record["host"].as_str().map(str::to_owned))
        .collect()
}
/// Prepare a sandboxed command with no network access.
pub fn prepare(
    program: &str,
    args: &[String],
    workspace: &Path,
    read_roots: &[PathBuf],
    protected_paths: &[PathBuf],
) -> Result<PreparedSandbox> {
    prepare_with_policy(program, args, workspace, read_roots, protected_paths, &[])
}
/// Prepare a sandboxed command whose network access is limited to
/// `allowed_domains`.
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
        None,
    )
}
/// Outbound network policy for a sandboxed coding command. Filesystem rules
/// are the same under both.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Network {
    /// No outbound connections. Blocked destinations are recorded in the
    /// status file; see [`read_network_denials`].
    #[default]
    None,
    /// Any outbound host, through the sandbox runtime's proxy. The runtime
    /// still refuses loopback, link-local and cloud-metadata addresses.
    Unrestricted,
}

/// Coding commands may use installed host Rust binaries, but never the user's
/// Cargo config/credentials. Cache/home writes stay in an owned temporary root.
/// Equivalent to [`prepare_coding_with_network`] with [`Network::None`].
pub fn prepare_coding(
    program: &str,
    args: &[String],
    workspace: &Path,
    read_roots: &[PathBuf],
    protected_paths: &[PathBuf],
) -> Result<PreparedSandbox> {
    prepare_coding_with_network(
        program,
        args,
        workspace,
        read_roots,
        protected_paths,
        Network::None,
    )
}

/// A coding command with an explicit [`Network`] policy. Offline commands
/// get Cargo's offline mode and a read-only view of the public crate cache;
/// with network access Cargo downloads into the command's own temporary
/// Cargo home instead, because the host cache stays read-only.
pub fn prepare_coding_with_network(
    program: &str,
    args: &[String],
    workspace: &Path,
    read_roots: &[PathBuf],
    protected_paths: &[PathBuf],
    network: Network,
) -> Result<PreparedSandbox> {
    prepare_internal(
        program,
        args,
        workspace,
        read_roots,
        protected_paths,
        &[],
        Some(network),
    )
}
fn prepare_internal(
    program: &str,
    args: &[String],
    workspace: &Path,
    read_roots: &[PathBuf],
    protected_paths: &[PathBuf],
    allowed_domains: &[String],
    coding: Option<Network>,
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
    // `tmp` is the child's writable TMPDIR; `control` holds the status file
    // and is denied to the child for both reading and writing.
    let temporary = tempfile::tempdir()?;
    let child_temporary = temporary.path().canonicalize()?.join("tmp");
    let control = temporary.path().canonicalize()?.join("control");
    std::fs::create_dir(&child_temporary)?;
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&control)?;
    let status_file = control.join("status");
    let (toolchain_read_roots, coding_environment) = match coding {
        Some(network) => rust_toolchain_scope(
            &child_temporary,
            &workspace,
            protected_paths,
            program,
            args,
            network,
        )?,
        None => (Vec::new(), std::collections::BTreeMap::new()),
    };
    let network = match coding.unwrap_or_default() {
        Network::None => "none",
        Network::Unrestricted => "unrestricted",
    };
    let spec = json!({"program":program,"args":args,"workspace":workspace,"read_roots":read_roots,"protected_paths":protected_paths,"trusted_paths":trusted_paths_with_node(&node)?,"toolchain_read_roots":toolchain_read_roots,"coding_environment":coding_environment,"temporary":child_temporary,"status_file":status_file,"allowed_domains":allowed_domains,"network":network});
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
    Ok(PreparedSandbox {
        command,
        temporary,
        status_file,
    })
}
/// The sandbox helper entrypoint. Release builds only use the copy packaged
/// with the application; the source tree is a fallback for debug builds.
pub fn helper_path() -> Result<PathBuf> {
    let executable = std::env::current_exe()?;
    let packaged = executable
        .parent()
        .context("Missing executable directory")?
        .join("../Resources/sandbox-helper/run.mjs");
    if packaged.is_file() {
        return Ok(packaged.canonicalize()?);
    }
    #[cfg(debug_assertions)]
    {
        let development = Path::new(env!("CARGO_MANIFEST_DIR")).join("sandbox-helper/run.mjs");
        if development.is_file() {
            return Ok(development.canonicalize()?);
        }
    }
    anyhow::bail!("Managed sandbox helper is missing from the application bundle")
}
fn rust_toolchain_scope(
    temporary: &Path,
    workspace: &Path,
    protected_paths: &[PathBuf],
    program: &str,
    args: &[String],
    network: Network,
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
    let offline = network == Network::None;
    if offline {
        environment.insert("CARGO_NET_OFFLINE".into(), "true".into());
    }
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
        if offline
            && (workspace.join("Cargo.toml").is_file()
                || program.contains("cargo")
                || args.iter().any(|arg| arg.contains("cargo")))
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
/// The trusted Node runtime that bootstraps the sandbox. See the module docs
/// for the search order and the ownership rule for discovered binaries.
pub fn runtime_node() -> Result<PathBuf> {
    let unavailable = |reason: String| -> anyhow::Error {
        SandboxUnavailable(format!(
            "{reason}. Install Node.js 22.12 or later, or set {NODE_OVERRIDE} to a trusted Node.js executable."
        ))
        .into()
    };
    let packaged = helper_path()?
        .parent()
        .context("Missing sandbox helper directory")?
        .join("runtime/bin/node");
    if executable_file(&packaged) {
        let packaged = packaged.canonicalize()?;
        verified_node(&packaged).map_err(|reason| {
            unavailable(format!(
                "Bundled Node.js {} was rejected: {reason}",
                packaged.display()
            ))
        })?;
        return Ok(packaged);
    }
    if let Some(configured) = std::env::var_os(NODE_OVERRIDE) {
        let configured = PathBuf::from(configured);
        if !configured.is_absolute() || !executable_file(&configured) {
            return Err(unavailable(format!(
                "{NODE_OVERRIDE}={} was rejected: it is not the absolute path of an executable file",
                configured.display()
            )));
        }
        let configured = configured.canonicalize()?;
        verified_node(&configured).map_err(|reason| {
            unavailable(format!(
                "{NODE_OVERRIDE}={} was rejected: {reason}",
                configured.display()
            ))
        })?;
        return Ok(configured);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .and_then(|home| home.canonicalize().ok());
    let mut rejected = Vec::new();
    for candidate in system_node_candidates() {
        match trusted_system_location(&candidate, home.as_deref())
            .and_then(|()| verified_node(&candidate))
        {
            Ok(()) => return Ok(candidate),
            Err(reason) => rejected.push(format!(
                "Node.js {} was rejected: {reason}",
                candidate.display()
            )),
        }
    }
    Err(unavailable(if rejected.is_empty() {
        "No Node.js was found on PATH".into()
    } else {
        rejected.join("; ")
    }))
}

fn system_node_candidates() -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = find_program("node").into_iter().collect();
    // Finder-launched macOS applications do not inherit the user's shell PATH.
    #[cfg(target_os = "macos")]
    candidates.extend(
        ["/opt/homebrew/bin/node", "/usr/local/bin/node"]
            .into_iter()
            .map(Path::new)
            .filter(|path| executable_file(path))
            .filter_map(|path| path.canonicalize().ok()),
    );
    candidates.dedup();
    candidates
}

/// Groups whose members can already act as root. Homebrew makes its Cellar
/// writable by `admin` on macOS (gid 80, the sudoers group), which grants no
/// one more than they already have.
#[cfg(unix)]
const ROOT_EQUIVALENT_GROUPS: &[u32] = &[
    0,
    #[cfg(target_os = "macos")]
    80,
];

/// A discovered runtime must not be replaceable by anyone other than the user
/// or root: the resolved binary and each directory above it, stopping before
/// `home` when the binary is inside it and at `/` otherwise, must be owned by
/// the current user or root and must not be world-writable or writable by a
/// group other than a root-equivalent one.
fn trusted_system_location(node: &Path, home: Option<&Path>) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // SAFETY: geteuid has no arguments or memory preconditions.
        let user = unsafe { libc::geteuid() };
        let stop = home.filter(|home| node.starts_with(home));
        for path in node.ancestors() {
            if stop.is_some_and(|home| path == home) {
                break;
            }
            let metadata = path
                .metadata()
                .with_context(|| format!("cannot inspect {}", path.display()))?;
            ensure!(
                metadata.uid() == user || metadata.uid() == 0,
                "{} is owned by another user (uid {})",
                path.display(),
                metadata.uid()
            );
            let mode = metadata.permissions().mode();
            ensure!(mode & 0o002 == 0, "{} is world-writable", path.display());
            ensure!(
                mode & 0o020 == 0 || ROOT_EQUIVALENT_GROUPS.contains(&metadata.gid()),
                "{} is group-writable (gid {})",
                path.display(),
                metadata.gid()
            );
        }
    }
    #[cfg(not(unix))]
    let _ = (node, home);
    Ok(())
}

fn verified_node(node: &Path) -> Result<()> {
    let version = node_version(node)?;
    ensure!(
        version >= MINIMUM_NODE,
        "it is version {}.{}, older than 22.12",
        version.0,
        version.1
    );
    Ok(())
}

/// `node --version`, cached per path and modification time so preparing a
/// command does not spawn an extra process every time.
fn node_version(node: &Path) -> Result<(u64, u64)> {
    use std::{
        collections::HashMap,
        sync::{Mutex, OnceLock},
        time::SystemTime,
    };
    type Cache = Mutex<HashMap<PathBuf, (SystemTime, (u64, u64))>>;
    static CACHE: OnceLock<Cache> = OnceLock::new();
    let modified = node.metadata()?.modified()?;
    let cache = CACHE.get_or_init(Default::default);
    if let Some((stamp, version)) = cache.lock().ok().and_then(|cache| cache.get(node).copied())
        && stamp == modified
    {
        return Ok(version);
    }
    let output = std::process::Command::new(node)
        .arg("--version")
        .env_clear()
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .with_context(|| format!("Could not run {}", node.display()))?;
    ensure!(
        output.status.success(),
        "{} --version failed",
        node.display()
    );
    let version = parse_node_version(&String::from_utf8_lossy(&output.stdout))
        .with_context(|| format!("{} reported an unrecognized version", node.display()))?;
    if let Ok(mut cache) = cache.lock() {
        cache.insert(node.to_owned(), (modified, version));
    }
    Ok(version)
}

fn parse_node_version(text: &str) -> Option<(u64, u64)> {
    let mut parts = text.trim().strip_prefix('v')?.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
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
/// Refuse writes into the helper, the host executable directory or the
/// trusted Node runtime.
pub fn ensure_writable_target(path: &Path) -> Result<()> {
    for protected in trusted_paths()? {
        ensure!(
            !path.starts_with(protected),
            "Tool cannot modify the application execution boundary"
        );
    }
    Ok(())
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
mod tests;

#[cfg(test)]
mod coding_scope_tests;
