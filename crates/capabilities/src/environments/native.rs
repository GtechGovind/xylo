//! Explicit argv only. SSH's remote shell boundary is quoted once; container
//! mutations are addressed by an inspected immutable ID and ownership labels.
use super::*;
use crate::process::{self, ManagedProcess, ProcessOutput, ProcessSpec};
use std::{
    collections::{BTreeMap, HashMap},
    sync::OnceLock,
};
use tokio::sync::Mutex;

pub(super) const OWNER: &str = "io.agent-workbench.installation";
pub(super) const PROFILE: &str = "io.agent-workbench.environment";
pub(super) const CREATED: &str = "io.agent-workbench.create-operation";
type Sessions = HashMap<(PathBuf, Uuid), ManagedProcess>;
fn sessions() -> &'static Mutex<Sessions> {
    static SESSIONS: OnceLock<Mutex<Sessions>> = OnceLock::new();
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
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
    let executable = match &record.profile.target {
        EnvironmentTarget::Ssh { executable, .. }
        | EnvironmentTarget::Container { executable, .. } => executable.clone(),
    };
    ensure!(
        process::validate_executable(&executable)? == executable,
        "The configured client path changed; review and save the environment before continuing"
    );
    Ok(ProcessSpec {
        executable,
        args,
        cwd: manager.root.clone(),
        env: BTreeMap::new(),
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
    let suffix = format!(
        "{:x}",
        Sha256::digest(format!("{}:{installation}", manager.root.display()))
    );
    let root = PathBuf::from("/tmp").join(format!(
        "workbench-{}-{}",
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
    if ssh_check(manager, record, cancel.clone()).await? {
        return Ok(());
    }
    #[cfg(unix)]
    {
        let mut args = ssh_args(manager, record, &cancel).await?;
        let host = args.pop().context("Missing SSH host")?;
        args.extend([
            "-M".into(),
            "-N".into(),
            "-oControlPersist=no".into(),
            "--".into(),
            host,
        ]);
        let mut master = ManagedProcess::spawn(&spec(manager, record, args, 15, 8192)?)?;
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
        "workbench-{}-{}",
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
