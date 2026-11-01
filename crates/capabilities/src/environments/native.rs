//! Explicit argv only. SSH's remote shell boundary is quoted once; container
//! mutations are addressed by an inspected immutable ID and ownership labels.
use super::*;
use crate::process::{self, ManagedProcess, ProcessOutput, ProcessSpec};
use std::{
    collections::{BTreeMap, HashMap},
    sync::OnceLock,
};
use tokio::sync::Mutex;

pub(super) const OWNER: &str = "com.qurkos.xylo.installation";
pub(super) const PROFILE: &str = "com.qurkos.xylo.environment";
pub(super) const CREATED: &str = "com.qurkos.xylo.create-operation";
type Sessions = HashMap<(PathBuf, Uuid), ManagedProcess>;
fn sessions() -> &'static Mutex<Sessions> {
    static SESSIONS: OnceLock<Mutex<Sessions>> = OnceLock::new();
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}
/// Per-profile connect serialization. Entries are tiny and bounded by the
/// number of saved profiles, so they are kept for the process lifetime.
async fn connect_gate(root: &Path, id: Uuid) -> std::sync::Arc<Mutex<()>> {
    static GATES: OnceLock<Mutex<HashMap<(PathBuf, Uuid), std::sync::Arc<Mutex<()>>>>> =
        OnceLock::new();
    GATES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .await
        .entry((root.to_path_buf(), id))
        .or_default()
        .clone()
}
pub(super) fn active(root: &Path) -> usize {
    let Ok(mut registry) = sessions().try_lock() else {
        return 1;
    };
    registry.retain(|_, process| process.is_running().unwrap_or(true));
    registry
        .keys()
        .filter(|(directory, _)| directory == root)
        .count()
}

pub(super) async fn shutdown(root: &Path) -> Result<()> {
    let keys = sessions()
        .lock()
        .await
        .keys()
        .filter(|(directory, _)| directory == root)
        .cloned()
        .collect::<Vec<_>>();
    for key in keys {
        let process = sessions().lock().await.remove(&key);
        if let Some(mut process) = process {
            process.stop().await?;
        }
    }
    Ok(())
}
pub(super) fn quote(value: &str) -> Result<String> {
    ensure!(
        !value.contains(['\0', '\r', '\n']),
        "Remote arguments must not contain NUL or line breaks"
    );
    Ok(format!("'{}'", value.replace('\'', "'\\''")))
}
pub(super) fn validate_command(command: &[String]) -> Result<()> {
    ensure!(
        !command.is_empty() && command.len() <= 128 && !command[0].is_empty(),
        "Choose a program and at most 127 arguments"
    );
    ensure!(
        command
            .iter()
            .all(|value| value.len() <= 8192 && !value.contains(['\0', '\r', '\n']))
            && command.iter().map(String::len).sum::<usize>() <= 64 * 1024,
        "Environment command exceeds its argument budget"
    );
    Ok(())
}
pub(super) fn spec(
    manager: &Manager,
    record: &EnvironmentRecord,
    args: Vec<String>,
    timeout: u64,
    limit: usize,
) -> Result<ProcessSpec> {
    // Clients get the baseline environment plus only what reaches their
    // own agent or daemon; provider keys and other app variables never do.
    let mut env = BTreeMap::new();
    let executable = match &record.profile.target {
        EnvironmentTarget::Ssh { executable, .. } => {
            process::inherit(&mut env, &["SSH_AUTH_SOCK"]);
            executable.clone()
        }
        EnvironmentTarget::Container { executable, .. } => {
            process::inherit(&mut env, &["DOCKER_HOST", "CONTAINER_HOST"]);
            executable.clone()
        }
    };
    ensure!(
        process::validate_executable(&executable)? == executable,
        "The configured client path changed; review and save the environment before continuing"
    );
    Ok(ProcessSpec {
        executable,
        args,
        cwd: manager.root.clone(),
        env,
        timeout: Duration::from_secs(timeout),
        output_limit: limit,
    })
}
pub(super) async fn run(
    manager: &Manager,
    record: &EnvironmentRecord,
    args: Vec<String>,
    timeout: u64,
    cancel: CancellationToken,
) -> Result<ProcessOutput> {
    run_limit(manager, record, args, timeout, 8192, cancel).await
}
pub(super) async fn run_limit(
    manager: &Manager,
    record: &EnvironmentRecord,
    args: Vec<String>,
    timeout: u64,
    limit: usize,
    cancel: CancellationToken,
) -> Result<ProcessOutput> {
    process::run(spec(manager, record, args, timeout, limit)?, cancel).await
}
#[cfg(unix)]
pub(super) async fn socket(
    manager: &Manager,
    id: Uuid,
    cancel: &CancellationToken,
) -> Result<PathBuf> {
    let installation = manager.installation(cancel).await?;
    // The long user data path cannot fit sockaddr_un on macOS. This private
    // per-installation directory contains only multiplex control sockets.
    let suffix = hex::encode(Sha256::digest(format!(
        "{}:{installation}",
        manager.root.display()
    )));
    let root = PathBuf::from("/tmp").join(format!(
        "xylo-{}-{}",
        unsafe { libc::geteuid() },
        &suffix[..12]
    ));
    storage::private_directory(&root)?;
    let path = root.join(format!("s-{}", id.simple()));
    ensure!(
        path.as_os_str().as_encoded_bytes().len() <= 100,
        "The SSH control socket path exceeds the platform limit"
    );
    Ok(path)
}
async fn ssh_args(
    manager: &Manager,
    record: &EnvironmentRecord,
    cancel: &CancellationToken,
) -> Result<Vec<String>> {
    let EnvironmentTarget::Ssh {
        host,
        user,
        port,
        known_hosts,
        identity_file,
        ..
    } = &record.profile.target
    else {
        bail!("Choose an SSH environment")
    };
    // Ignore user/system configuration (ProxyCommand, LocalCommand, forwarding).
    // Authentication may use an existing agent; this service never reads keys.
    let mut args = vec![
        "-F".into(),
        if cfg!(windows) {
            "NUL".into()
        } else {
            "/dev/null".into()
        },
        "-T".into(),
        "-oBatchMode=yes".into(),
        "-oStrictHostKeyChecking=yes".into(),
        "-oGlobalKnownHostsFile=none".into(),
        format!(
            "-oUserKnownHostsFile=\"{}\"",
            known_hosts
                .to_string_lossy()
                .replace('\\', "/")
                .replace('"', "\\\"")
        ),
        "-oUpdateHostKeys=no".into(),
        "-oConnectTimeout=10".into(),
        "-oConnectionAttempts=1".into(),
        "-oClearAllForwardings=yes".into(),
        "-oForwardAgent=no".into(),
        "-oPermitLocalCommand=no".into(),
        "-p".into(),
        port.to_string(),
        "-l".into(),
        user.clone(),
    ];
    if let Some(identity) = identity_file {
        args.extend([
            "-i".into(),
            identity.to_string_lossy().into_owned(),
            "-oIdentitiesOnly=yes".into(),
        ]);
    }
    #[cfg(unix)]
    args.extend([
        "-S".into(),
        socket(manager, record.profile.id, cancel)
            .await?
            .to_string_lossy()
            .into_owned(),
    ]);
    #[cfg(not(unix))]
    {
        let _ = (manager, cancel);
    }
    args.push(host.clone());
    Ok(args)
}
pub(super) async fn ssh_check(
    manager: &Manager,
    record: &EnvironmentRecord,
    cancel: CancellationToken,
) -> Result<bool> {
    let mut args = ssh_args(manager, record, &cancel).await?;
    let host = args.pop().context("Missing SSH host")?;
    #[cfg(unix)]
    args.extend(["-O".into(), "check".into(), "--".into(), host]);
    #[cfg(not(unix))]
    args.extend(["--".into(), host, "exec 'true'".into()]);
    let result = run(manager, record, args, 5, cancel).await?;
    Ok(result.success)
}
pub(super) async fn ssh_connect(
    manager: &Manager,
    record: &EnvironmentRecord,
    cancel: CancellationToken,
) -> Result<()> {
    // One connect per profile at a time: the check, the stale-socket removal
    // and the spawn must not interleave with another connect, which could
    // delete the socket a concurrent master has just bound. (The only caller
    // also holds the profile's cross-process lease.)
    let gate = connect_gate(&manager.root, record.profile.id).await;
    let _connecting = gate.lock().await;
    if ssh_check(manager, record, cancel.clone()).await? {
        return Ok(());
    }
    #[cfg(unix)]
    {
        remove_stale_socket(&socket(manager, record.profile.id, &cancel).await?);
        let mut args = ssh_args(manager, record, &cancel).await?;
        let host = args.pop().context("Missing SSH host")?;
        args.extend([
            "-M".into(),
            "-N".into(),
            "-oControlPersist=no".into(),
            "--".into(),
            host,
        ]);
        let mut master =
            ManagedProcess::spawn_tethered(&tethered(spec(manager, record, args, 15, 8192)?))?;
        let result = async {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
            loop {
                ensure!(!cancel.is_cancelled(), "SSH connection cancelled");
                ensure!(
                    master.is_running()?,
                    "The SSH client exited before a verified session was established"
                );
                if ssh_check(manager, record, cancel.clone()).await? {
                    return Ok(());
                }
                ensure!(
                    tokio::time::Instant::now() < deadline,
                    "SSH session did not become ready"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
        .await;
        if let Err(error) = result {
            master.stop().await?;
            return Err(error);
        }
        let old = sessions()
            .lock()
            .await
            .insert((manager.root.clone(), record.profile.id), master);
        if let Some(mut old) = old {
            old.stop().await?;
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        ensure!(
            ssh_check(manager, record, cancel).await?,
            "The SSH host could not be verified noninteractively"
        );
        Ok(())
    }
}
/// Runs the SSH master under a POSIX shell that ends it when its owner ends.
/// The master runs in its own process group, so it would otherwise outlive an
/// owner that exits without stopping it (a crash, SIGKILL, or an exit that
/// skips destructors), holding the connection open with no one to close it.
/// The shell's stdin is the owner's tether pipe: a watcher reads it until end
/// of input, which the kernel delivers when the owner's write end closes, and
/// then sends SIGTERM so the master also removes its control socket. The
/// watcher uses only the `read` builtin, so it starts no other process. When
/// the master exits first, the shell stops the watcher before exiting with
/// the master's status; a watcher left waiting could otherwise signal an
/// unrelated process that later reuses the master's PID. A failed connection
/// still shows as an exited client. `ControlPersist` with a timeout is no substitute: it
/// makes ssh fork the master into the background, outside this tether and
/// the process group the owner stops.
///
/// Windows has no multiplexed master; there, every native process is placed
/// in a job object that closes with its owner.
#[cfg(unix)]
const MASTER_TETHER: &str = r#"exec 3<&0 0</dev/null
"$@" 3<&- &
master=$!
{ while read -r _ <&3; do :; done; kill -TERM "$master" 2>/dev/null; } &
watcher=$!
exec 3<&-
wait "$master"
status=$?
kill "$watcher" 2>/dev/null
exit "$status""#;

#[cfg(unix)]
pub(super) fn tethered(mut spec: ProcessSpec) -> ProcessSpec {
    let client = std::mem::replace(&mut spec.executable, PathBuf::from("/bin/sh"));
    let mut args = vec![
        "-c".into(),
        MASTER_TETHER.into(),
        "xylo-ssh-master".into(),
        client.to_string_lossy().into_owned(),
    ];
    args.append(&mut spec.args);
    spec.args = args;
    spec
}

/// A master stopped with SIGKILL leaves its control socket behind, and ssh
/// refuses to listen on an existing path ("ControlSocket already exists,
/// disabling multiplexing"). Only a socket nothing listens on is removed.
#[cfg(unix)]
pub(super) fn remove_stale_socket(path: &Path) {
    use std::os::unix::fs::FileTypeExt;
    let is_socket =
        std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_socket());
    if is_socket
        && std::os::unix::net::UnixStream::connect(path)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::ConnectionRefused)
    {
        let _ = std::fs::remove_file(path);
    }
}

pub(super) async fn ssh_disconnect(
    manager: &Manager,
    record: &EnvironmentRecord,
    cancel: CancellationToken,
) -> Result<()> {
    #[cfg(unix)]
    {
        let mut args = ssh_args(manager, record, &cancel).await?;
        let host = args.pop().context("Missing SSH host")?;
        args.extend(["-O".into(), "exit".into(), "--".into(), host]);
        let _ = run(manager, record, args, 5, cancel.clone()).await?;
        let process = sessions()
            .lock()
            .await
            .remove(&(manager.root.clone(), record.profile.id));
        if let Some(mut process) = process {
            process.stop().await?;
        }
        ensure!(
            !ssh_check(manager, record, cancel).await?,
            "SSH session is still active; check its status"
        );
    }
    #[cfg(not(unix))]
    {
        let _ = (manager, record, cancel);
    }
    Ok(())
}
pub(super) async fn execution_args(
    manager: &Manager,
    record: &EnvironmentRecord,
    command: &[String],
    cancel: &CancellationToken,
) -> Result<Vec<String>> {
    validate_command(command)?;
    match &record.profile.target {
        EnvironmentTarget::Ssh { .. } => {
            let mut args = ssh_args(manager, record, cancel).await?;
            let host = args.pop().context("Missing SSH host")?;
            #[cfg(unix)]
            args.push("-oProxyCommand=false".into()); // Fail if the checked master disappears.
            args.extend([
                "--".into(),
                host,
                format!(
                    "exec {}",
                    command
                        .iter()
                        .map(|value| quote(value))
                        .collect::<Result<Vec<_>>>()?
                        .join(" ")
                ),
            ]);
            Ok(args)
        }
        EnvironmentTarget::Container { .. } => {
            let id = record
                .container_id
                .as_ref()
                .context("Connect this container before executing a command")?;
            let mut args = vec!["container".into(), "exec".into(), "--".into(), id.clone()];
            args.extend_from_slice(command);
            Ok(args)
        }
    }
}
#[derive(Clone, Debug)]
pub(super) struct Inspection {
    pub id: String,
    pub running: bool,
    pub labels: BTreeMap<String, String>,
}
pub(super) async fn inspect(
    manager: &Manager,
    record: &EnvironmentRecord,
    target: &str,
    cancel: CancellationToken,
) -> Result<Option<Inspection>> {
    let args = vec![
        "container".into(),
        "inspect".into(),
        "--format".into(),
        "{{json .}}".into(),
        "--".into(),
        target.into(),
    ];
    let result = run_limit(manager, record, args, 10, 64 * 1024, cancel).await?;
    if !result.success {
        // Never interpret arbitrary daemon/transport/auth errors as absence.
        let error = result.stderr.to_ascii_lowercase();
        if error.contains("no such container") || error.contains("no container with name or id") {
            return Ok(None);
        }
        bail!(
            "The container service could not confirm this target: {}",
            result.stderr.chars().take(512).collect::<String>()
        );
    }
    ensure!(
        !result.truncated,
        "Container identity response exceeded its budget"
    );
    let value: Value = serde_json::from_str(result.stdout.trim())
        .context("The container service returned invalid inspection data")?;
    let id = value
        .get("Id")
        .or_else(|| value.get("ID"))
        .and_then(Value::as_str)
        .context("Container has no immutable ID")?;
    ensure!(
        id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "Container service returned an invalid immutable ID"
    );
    let running = value
        .pointer("/State/Running")
        .and_then(Value::as_bool)
        .context("Container has no running state")?;
    let labels = serde_json::from_value(
        value
            .pointer("/Config/Labels")
            .filter(|value| !value.is_null())
            .cloned()
            .unwrap_or(json!({})),
    )?;
    Ok(Some(Inspection {
        id: id.into(),
        running,
        labels,
    }))
}
pub(super) fn container_name(installation: Uuid, id: Uuid) -> String {
    format!(
        "xylo-{}-{}",
        &installation.simple().to_string()[..8],
        id.simple()
    )
}

pub(super) fn owned(inspection: &Inspection, installation: Uuid, id: Uuid) -> Result<()> {
    ensure!(
        inspection.labels.get(OWNER) == Some(&installation.to_string())
            && inspection.labels.get(PROFILE) == Some(&id.to_string())
            && inspection
                .labels
                .get(CREATED)
                .is_some_and(|value| Uuid::parse_str(value).is_ok()),
        "This container is not owned by this Xylo installation and profile"
    );
    Ok(())
}
