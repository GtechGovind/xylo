//! Bounded native process execution. No shell is used for local helpers.
//!
//! Native helpers (ssh, docker/podman, llama-server, whisper, cmake) never
//! inherit the application environment, which can hold provider API keys.
//! Each starts from an empty environment plus [`baseline_environment`] and the
//! variables its spec adds explicitly (for example `SSH_AUTH_SOCK` for ssh).

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

/// Parent variables every native helper may inherit: executable lookup, the
/// user's identity and locale, and the temporary directory. `LC_*` locale
/// categories are added by [`baseline_environment`].
const BASELINE: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "TMPDIR",
    #[cfg(windows)]
    "SYSTEMROOT",
    #[cfg(windows)]
    "WINDIR",
    #[cfg(windows)]
    "TEMP",
    #[cfg(windows)]
    "TMP",
    #[cfg(windows)]
    "USERPROFILE",
    #[cfg(windows)]
    "LOCALAPPDATA",
    #[cfg(windows)]
    "APPDATA",
    #[cfg(windows)]
    "PATHEXT",
    #[cfg(windows)]
    "COMSPEC",
];

/// The allowlisted subset of this process's environment that native helpers
/// start with.
pub fn baseline_environment() -> BTreeMap<String, String> {
    std::env::vars()
        .filter(|(name, _)| BASELINE.contains(&name.as_str()) || name.starts_with("LC_"))
        .collect()
}

/// Copy the named parent variables, when set, into `environment`. Callers use
/// this for the few variables one helper genuinely needs, such as
/// `SSH_AUTH_SOCK` for ssh or `DOCKER_HOST` for container clients.
pub fn inherit(environment: &mut BTreeMap<String, String>, names: &[&str]) {
    for name in names {
        if let Ok(value) = std::env::var(name) {
            environment.insert((*name).to_owned(), value);
        }
    }
}

/// A native helper invocation. `env` is added on top of
/// [`baseline_environment`]; nothing else is inherited.
#[derive(Clone, Debug)]
pub struct ProcessSpec {
    pub executable: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
    pub timeout: Duration,
    pub output_limit: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessOutput {
    pub success: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
}

pub fn validate_executable(path: &std::path::Path) -> Result<PathBuf> {
    ensure!(
        path.is_absolute(),
        "Helper executable must be an explicit absolute path"
    );
    let path = std::fs::canonicalize(path)
        .context("Native helper is not installed at the configured path")?;
    ensure!(path.is_file(), "Helper executable is not a regular file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            std::fs::metadata(&path)?.permissions().mode() & 0o111 != 0,
            "Helper is not executable"
        );
    }
    Ok(path)
}

fn command(spec: &ProcessSpec) -> Result<Command> {
    let executable = validate_executable(&spec.executable)?;
    ensure!(spec.cwd.is_dir(), "Process working directory is missing");
    ensure!(
        !spec.timeout.is_zero() && spec.timeout <= Duration::from_secs(3600),
        "Invalid process timeout"
    );
    ensure!(
        spec.output_limit > 0 && spec.output_limit <= 16 * 1024 * 1024,
        "Invalid process output limit"
    );
    ensure!(
        spec.args.iter().all(|v| !v.contains('\0')),
        "NUL in process arguments"
    );
    let mut cmd = Command::new(executable);
    // Background pipes must not create a separate Windows console.
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    cmd.args(&spec.args)
        .current_dir(&spec.cwd)
        .env_clear()
        .envs(baseline_environment())
        .envs(&spec.env)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.as_std_mut().process_group(0);
    }
    Ok(cmd)
}

/// Drains even after reaching the capture budget, avoiding blocked helper pipes.
async fn capture(mut pipe: impl AsyncRead + Unpin, limit: usize) -> (Vec<u8>, bool) {
    let mut bytes = Vec::with_capacity(limit.min(8192));
    let mut buffer = [0; 8192];
    let mut truncated = false;
    loop {
        match pipe.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let count = (limit - bytes.len()).min(n);
                bytes.extend_from_slice(&buffer[..count]);
                truncated |= count < n;
            }
        }
    }
    (bytes, truncated)
}

pub struct ManagedProcess {
    child: Child,
    pid: Option<u32>,
    stdout: Option<JoinHandle<(Vec<u8>, bool)>>,
    stderr: Option<JoinHandle<(Vec<u8>, bool)>>,
    #[cfg(windows)]
    job: WindowsJob,
}

impl ManagedProcess {
    pub fn spawn(spec: &ProcessSpec) -> Result<Self> {
        let mut process = Self::spawn_uncaptured(spec)?;
        process.stdout = process
            .child
            .stdout
            .take()
            .map(|pipe| tokio::spawn(capture(pipe, spec.output_limit)));
        process.stderr = process
            .child
            .stderr
            .take()
            .map(|pipe| tokio::spawn(capture(pipe, spec.output_limit)));
        Ok(process)
    }

    /// Internal opt-in boundary for durable environment output. Generic callers
    /// retain the same bounded preview readers through `spawn` above.
    pub(crate) fn spawn_uncaptured(spec: &ProcessSpec) -> Result<Self> {
        let child = command(spec)?
            .spawn()
            .context("Failed to start native helper")?;
        #[cfg(windows)]
        let mut child = child;
        #[cfg(windows)]
        let job = match WindowsJob::attach(
            child
                .raw_handle()
                .context("Native process has no Windows handle")?,
        ) {
            Ok(job) => job,
            Err(error) => {
                let _ = child.start_kill();
                return Err(error);
            }
        };
        let pid = child.id();
        Ok(Self {
            child,
            pid,
            stdout: None,
            stderr: None,
            #[cfg(windows)]
            job,
        })
    }

    pub fn is_running(&mut self) -> Result<bool> {
        if self.child.try_wait()?.is_none() {
            return Ok(true);
        }
        // Reap descendants with the leader, and never retain a completed PID
        // in a long-lived service registry where Drop could act on it later.
        self.kill_tree();
        self.pid = None;
        Ok(false)
    }
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    pub(crate) fn take_stdout(&mut self) -> Option<tokio::process::ChildStdout> {
        self.child.stdout.take()
    }
    pub(crate) fn take_stderr(&mut self) -> Option<tokio::process::ChildStderr> {
        self.child.stderr.take()
    }
    pub(crate) async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.child.wait().await
    }
    pub(crate) fn finish_tree(&mut self) {
        self.kill_tree();
        self.pid = None;
    }

    pub async fn stop(&mut self) -> Result<()> {
        self.kill_tree();
        let _ = self.child.kill().await;
        let _ = tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await;
        for handle in [&mut self.stdout, &mut self.stderr] {
            if let Some(task) = handle.take() {
                task.abort();
            }
        }
        self.pid = None;
        Ok(())
    }

    fn kill_tree(&self) {
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            // The child owns a fresh process group. Kill descendants as well.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
        #[cfg(windows)]
        self.job.terminate();
    }
}

impl Drop for ManagedProcess {
    fn drop(&mut self) {
        self.kill_tree();
        let _ = self.child.start_kill();
        if let Some(task) = self.stdout.take() {
            task.abort();
        }
        if let Some(task) = self.stderr.take() {
            task.abort();
        }
    }
}

pub async fn run(spec: ProcessSpec, cancel: CancellationToken) -> Result<ProcessOutput> {
    let mut process = ManagedProcess::spawn(&spec)?;
    let status = tokio::select! {
        result = process.child.wait() => result?,
        _ = cancel.cancelled() => { process.stop().await?; anyhow::bail!("Native command cancelled"); },
        _ = tokio::time::sleep(spec.timeout) => { process.stop().await?; anyhow::bail!("Native command exceeded {} seconds", spec.timeout.as_secs()); },
    };
    process.kill_tree();
    process.pid = None;
    let output_timeout = Duration::from_secs(2);
    async fn join(task: Option<JoinHandle<(Vec<u8>, bool)>>, timeout: Duration) -> (Vec<u8>, bool) {
        if let Some(mut task) = task {
            match tokio::time::timeout(timeout, &mut task).await {
                Ok(Ok(v)) => v,
                _ => {
                    task.abort();
                    (Vec::new(), true)
                }
            }
        } else {
            (Vec::new(), false)
        }
    }
    let (out, err) = tokio::join!(
        join(process.stdout.take(), output_timeout),
        join(process.stderr.take(), output_timeout)
    );
    Ok(ProcessOutput {
        success: status.success(),
        exit_code: status.code(),
        stdout: String::from_utf8_lossy(&out.0).into_owned(),
        stderr: String::from_utf8_lossy(&err.0).into_owned(),
        truncated: out.1 || err.1,
    })
}

#[cfg(windows)]
struct WindowsJob(usize);

#[cfg(windows)]
impl WindowsJob {
    fn attach(process: std::os::windows::io::RawHandle) -> Result<Self> {
        use windows_sys::Win32::{Foundation::CloseHandle, System::JobObjects::*};
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            ensure!(
                !job.is_null(),
                "Could not create a process job: {}",
                std::io::Error::last_os_error()
            );
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&limits) as u32,
            ) == 0
                || AssignProcessToJobObject(job, process) == 0
            {
                let error = std::io::Error::last_os_error();
                CloseHandle(job);
                anyhow::bail!("Could not isolate native process in a job: {error}");
            }
            Ok(Self(job as usize))
        }
    }
    fn terminate(&self) {
        unsafe {
            windows_sys::Win32::System::JobObjects::TerminateJobObject(self.0 as _, 1);
        }
    }
}

#[cfg(windows)]
impl Drop for WindowsJob {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0 as _);
        }
    }
}

#[cfg(test)]
mod tests;
