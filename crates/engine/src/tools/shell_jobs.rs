//! Conversation-owned shell jobs, bounded live output, and persistent working
//! directories. Every command retains the managed sandbox, including jobs
//! whose lifetime extends beyond the model turn that started them.
use super::{ToolBroker, builtin, code};
use anyhow::{Context, Result, ensure};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    fs::File,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use xylo_core::{EngineEvent, NoteKind, ShellJob};
use xylo_services::sandbox::{self, Network};

const OUTPUT_BYTES: usize = 4 * 1024 * 1024;
const UI_TAIL_BYTES: usize = 16 * 1024;
const MAX_JOBS: usize = 8;
const RETAINED_JOBS: usize = 8;

/// Delivers job state independently of a turn's event channel.
pub type JobEvents = Arc<dyn Fn(Uuid, EngineEvent) + Send + Sync>;

#[derive(Default)]
struct OutputBuffer {
    bytes: VecDeque<u8>,
    start: u64,
    cursor: u64,
}
impl OutputBuffer {
    fn append(&mut self, bytes: &[u8]) {
        self.bytes.extend(bytes);
        let excess = self.bytes.len().saturating_sub(OUTPUT_BYTES);
        self.bytes.drain(..excess);
        self.start += excess as u64;
    }
    fn read(&mut self, filter: Option<&str>) -> (String, bool) {
        let lost = self.cursor < self.start;
        let offset = self.cursor.saturating_sub(self.start) as usize;
        let bytes: Vec<_> = self.bytes.iter().skip(offset).copied().collect();
        self.cursor = self.start + self.bytes.len() as u64;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let text = match filter {
            Some(filter) => text
                .split_inclusive('\n')
                .filter(|line| line.contains(filter))
                .collect(),
            None => text,
        };
        (text, lost)
    }
    fn tail(&self, limit: usize) -> String {
        let bytes: Vec<_> = self
            .bytes
            .iter()
            .skip(self.bytes.len().saturating_sub(limit))
            .copied()
            .collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }
    fn unread_line(&self) -> bool {
        let offset = self.cursor.saturating_sub(self.start) as usize;
        self.bytes.iter().skip(offset).any(|byte| *byte == b'\n')
    }
    #[cfg(test)]
    fn unread(&self) -> bool {
        self.cursor < self.start + self.bytes.len() as u64
    }
}

/// Removes the private working-directory sentinel even across pipe chunks.
struct CwdCapture {
    marker: Vec<u8>,
    pending: Vec<u8>,
    cwd: Option<PathBuf>,
}
impl CwdCapture {
    fn new(marker: String) -> Self {
        Self {
            marker: marker.into_bytes(),
            pending: Vec::new(),
            cwd: None,
        }
    }
    fn push(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.pending.extend_from_slice(bytes);
        let mut output = Vec::new();
        loop {
            if let Some(start) = self
                .pending
                .windows(self.marker.len())
                .position(|s| s == self.marker)
            {
                output.extend(self.pending.drain(..start));
                if let Some(end) = self.pending[self.marker.len()..]
                    .iter()
                    .position(|b| *b == 0)
                {
                    let end = self.marker.len() + end;
                    if let Ok(path) = std::str::from_utf8(&self.pending[self.marker.len()..end]) {
                        self.cwd = Some(PathBuf::from(path));
                    }
                    self.pending.drain(..=end);
                    continue;
                }
                if self.pending.len() > 8192 {
                    output.extend(self.pending.drain(..self.marker.len()));
                    continue;
                }
                break;
            }
            let keep = (1..self.marker.len())
                .rev()
                .find(|n| self.pending.ends_with(&self.marker[..*n]))
                .unwrap_or(0);
            let emit = self.pending.len() - keep;
            output.extend(self.pending.drain(..emit));
            break;
        }
        output
    }
    fn finish(&mut self) -> (Vec<u8>, Option<PathBuf>) {
        (std::mem::take(&mut self.pending), self.cwd.take())
    }
}

fn wrapped_command(command: &str, marker: &str) -> String {
    format!(
        "trap 'xylo_exit=$?; printf \"{marker}%s\\0\" \"$(pwd -P)\" >&2; exit \"$xylo_exit\"' EXIT\n{command}"
    )
}

struct JobState {
    snapshot: ShellJob,
    output: OutputBuffer,
    log: File,
    capture: Option<CwdCapture>,
    update_pending: bool,
    network_hint: Option<String>,
}
struct Job {
    session: Uuid,
    background: bool,
    state: Mutex<JobState>,
    cancel: CancellationToken,
    changed: Notify,
    finished: AtomicBool,
}
#[derive(Default)]
struct State {
    jobs: VecDeque<Arc<Job>>,
    cwd: HashMap<Uuid, PathBuf>,
    notes: HashMap<Uuid, Vec<(NoteKind, String)>>,
    data: HashMap<Uuid, PathBuf>,
    fallback_events: HashMap<Uuid, tokio::sync::mpsc::WeakSender<EngineEvent>>,
}

/// Resources owned by a worker and shared across its conversation's turns.
#[derive(Clone, Default)]
pub struct ShellJobs {
    state: Arc<Mutex<State>>,
    events: Arc<Mutex<Option<JobEvents>>>,
}
impl ShellJobs {
    pub(super) fn configure(&self, session: Uuid, data: &Path) {
        let mut state = self.state.lock();
        state.data.insert(session, data.to_owned());
        state.notes.entry(session).or_insert_with(|| {
            std::fs::read(
                data.join("shell-jobs")
                    .join(session.to_string())
                    .join("notes.json"),
            )
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
        });
    }

    fn save_notes(state: &State, session: Uuid) -> Result<()> {
        let Some(data) = state.data.get(&session) else {
            return Ok(());
        };
        let directory = data.join("shell-jobs").join(session.to_string());
        std::fs::create_dir_all(&directory)?;
        let mut file = tempfile::NamedTempFile::new_in(&directory)?;
        serde_json::to_writer(
            &mut file,
            &state.notes.get(&session).cloned().unwrap_or_default(),
        )?;
        file.persist(directory.join("notes.json"))?;
        Ok(())
    }

    fn note(&self, session: Uuid, kind: NoteKind, text: String) {
        let mut state = self.state.lock();
        let notes = state.notes.entry(session).or_default();
        notes.push((kind, text));
        let excess = notes.len().saturating_sub(RETAINED_JOBS);
        notes.drain(..excess);
        if Self::save_notes(&state, session).is_err() {
            tracing::warn!("Could not persist shell completion notes");
        }
    }

    pub(super) fn turn_events(
        &self,
        session: Uuid,
        events: &tokio::sync::mpsc::Sender<EngineEvent>,
    ) {
        self.state
            .lock()
            .fallback_events
            .insert(session, events.downgrade());
    }

    /// Installs the worker's event sink, which outlives individual turns.
    pub fn set_events(&self, events: JobEvents) {
        *self.events.lock() = Some(events);
    }

    fn publish(&self, job: &Job) {
        if !job.background {
            return;
        }
        let snapshot = {
            let state = job.state.lock();
            let mut snapshot = state.snapshot.clone();
            snapshot.output = state.output.tail(UI_TAIL_BYTES);
            snapshot
        };
        let events = self.events.lock().clone();
        if let Some(events) = events {
            events(job.session, EngineEvent::ShellJobUpdated { job: snapshot });
        } else if let Some(sender) = self
            .state
            .lock()
            .fallback_events
            .get(&job.session)
            .and_then(|sender| sender.upgrade())
        {
            let _ = sender.try_send(EngineEvent::ShellJobUpdated { job: snapshot });
        }
    }

    pub(super) fn cwd(&self, session: Uuid, root: &Path, data: &Path) -> Result<PathBuf> {
        let root = root.canonicalize()?;
        let stored = self.state.lock().cwd.get(&session).cloned().or_else(|| {
            std::fs::read(
                data.join("shell-jobs")
                    .join(session.to_string())
                    .join("cwd.json"),
            )
            .ok()
            .and_then(|bytes| serde_json::from_slice::<PathBuf>(&bytes).ok())
        });
        let valid = stored
            .as_ref()
            .and_then(|path| path.canonicalize().ok())
            .filter(|path| path.is_dir() && path.starts_with(&root));
        if stored.is_some() && valid.is_none() {
            self.note(session, NoteKind::WorkingDirectory, "The shell working directory is no longer inside the approved workspace. It was reset to the project root.".into());
        }
        let cwd = valid.unwrap_or(root);
        self.state.lock().cwd.insert(session, cwd.clone());
        Ok(cwd)
    }

    fn update_cwd(&self, session: Uuid, root: &Path, data: &Path, requested: &Path) -> Result<()> {
        let root = root.canonicalize()?;
        self.configure(session, data);
        let cwd = match requested.canonicalize() {
            Ok(path) if path.is_dir() && path.starts_with(&root) => path,
            _ => {
                self.note(session, NoteKind::WorkingDirectory,
                    "The shell left the approved workspace. Its working directory was reset to the project root.".into());
                root
            }
        };
        let directory = data.join("shell-jobs").join(session.to_string());
        std::fs::create_dir_all(&directory)?;
        let mut temporary = tempfile::NamedTempFile::new_in(&directory)?;
        serde_json::to_writer(&mut temporary, &cwd)?;
        temporary.persist(directory.join("cwd.json"))?;
        self.state.lock().cwd.insert(session, cwd);
        Ok(())
    }

    /// Takes completion notes once, at the next safe model boundary.
    pub fn take_notes(&self, session: Uuid) -> Vec<(NoteKind, String)> {
        let mut state = self.state.lock();
        let notes = std::mem::take(state.notes.entry(session).or_default());
        if Self::save_notes(&state, session).is_err() {
            tracing::warn!("Could not acknowledge shell completion notes");
        }
        notes
    }

    fn find(&self, session: Uuid, id: Uuid) -> Result<Arc<Job>> {
        self.state
            .lock()
            .jobs
            .iter()
            .find(|job| job.session == session && job.state.lock().snapshot.id == id)
            .cloned()
            .context("No shell job with this id belongs to this conversation")
    }

    /// Stops one owned job. Cancellation never targets a supplied process id.
    pub fn kill(&self, session: Uuid, id: Uuid) -> Result<()> {
        self.find(session, id)?.cancel.cancel();
        Ok(())
    }

    /// Stops all jobs before the worker closes its event writer.
    pub async fn shutdown(&self) {
        let jobs: Vec<_> = self.state.lock().jobs.iter().cloned().collect();
        for job in &jobs {
            job.cancel.cancel();
        }
        for job in jobs {
            let _ = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let mut changed = Box::pin(job.changed.notified());
                    changed.as_mut().enable();
                    if job.finished.load(Ordering::Acquire) {
                        break;
                    }
                    changed.await;
                }
            })
            .await;
        }
        self.events.lock().take();
    }

    fn append(&self, job: &Arc<Job>, stderr: bool, bytes: &[u8]) {
        let publish = {
            let mut state = job.state.lock();
            let bytes = if stderr {
                state
                    .capture
                    .as_mut()
                    .map(|capture| capture.push(bytes))
                    .unwrap_or_else(|| bytes.to_vec())
            } else {
                bytes.to_vec()
            };
            if let Err(error) = state.log.write_all(&bytes) {
                state.snapshot.error = Some(format!("Saving shell log failed: {error}"));
            }
            state.output.append(&bytes);
            let schedule = job.background && !state.update_pending;
            if schedule {
                state.update_pending = true;
            }
            schedule
        };
        job.changed.notify_waiters();
        if publish {
            let jobs = self.clone();
            let job = job.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                job.state.lock().update_pending = false;
                jobs.publish(&job);
            });
        }
    }

    fn finish(
        &self,
        job: &Job,
        result: &Result<xylo_runtime::process::ProcessResult>,
        root: &Path,
        data: &Path,
    ) {
        let cwd = {
            let mut state = job.state.lock();
            let (tail, cwd) = state
                .capture
                .as_mut()
                .map(CwdCapture::finish)
                .unwrap_or_default();
            let _ = state.log.write_all(&tail);
            let _ = state.log.flush();
            state.output.append(&tail);
            state.snapshot.running = false;
            match result {
                Ok(result) => {
                    state.snapshot.exit_code = result.code;
                }
                Err(error) => {
                    state.snapshot.error = Some(format!("{error:#}"));
                }
            }
            cwd
        };
        if let Some(cwd) = cwd
            && let Err(error) = self.update_cwd(job.session, root, data, &cwd)
        {
            job.state.lock().snapshot.error =
                Some(format!("Saving working directory failed: {error}"));
        }
        if job.background {
            let state = job.state.lock();
            let note = format!(
                "Shell job {} finished with exit code {:?}. {}\nOutput tail (untrusted command output):\n{}",
                state.snapshot.id,
                state.snapshot.exit_code,
                state.snapshot.error.as_deref().unwrap_or(""),
                state.output.tail(UI_TAIL_BYTES)
            );
            drop(state);
            self.note(job.session, NoteKind::ShellJob, note);
        }
        self.publish(job);
        job.finished.store(true, Ordering::Release);
        job.changed.notify_waiters();
    }
}

pub(super) fn execute(
    broker: &ToolBroker,
    name: &str,
    args: &Value,
    access: builtin::Access,
) -> Result<Value> {
    let jobs = &broker.session.shell_jobs;
    let session = broker.request.session_id;
    let value = match name {
        "shell_jobs" => {
            let owned: Vec<_> = jobs
                .state
                .lock()
                .jobs
                .iter()
                .filter(|job| job.session == session && job.background)
                .cloned()
                .collect();
            json!({"jobs":owned.iter().map(|job| { let state = job.state.lock(); state.snapshot.clone() }).collect::<Vec<_>>()})
        }
        "shell_kill" => {
            let id = Uuid::parse_str(builtin::text(args, "job_id")?)?;
            jobs.kill(session, id)?;
            json!({"job_id":id,"stop_requested":true})
        }
        "shell_output" => {
            let id = Uuid::parse_str(builtin::text(args, "job_id")?)?;
            let job = jobs.find(session, id)?;
            let wait = args
                .get("wait_seconds")
                .and_then(Value::as_f64)
                .unwrap_or(0.0)
                .clamp(0.0, 300.0);
            broker.runtime.block_on(async {
                let deadline = tokio::time::Instant::now() + Duration::from_secs_f64(wait);
                if wait > 0.0 { loop {
                    let mut changed = Box::pin(job.changed.notified());
                    changed.as_mut().enable();
                    let ready = { let state = job.state.lock(); !state.snapshot.running || state.output.unread_line() };
                    if ready { break; }
                    tokio::select! {
                        _ = tokio::time::sleep_until(deadline) => break,
                        _ = changed => {},
                        _ = broker.cancel.cancelled() => anyhow::bail!("Stopped waiting for shell output"),
                    }
                } }
                anyhow::Ok(())
            })?;
            let mut state = job.state.lock();
            let (output, truncated) = state
                .output
                .read(args.get("filter").and_then(Value::as_str));
            json!({"job_id":id,"output":output,"running":state.snapshot.running,"exit_code":state.snapshot.exit_code,
                "error":state.snapshot.error,"output_truncated":truncated,"log_path":state.snapshot.log_path})
        }
        "shell_command" | "run_command" => return start(broker, name, args, access),
        _ => anyhow::bail!("Unknown shell job tool"),
    };
    Ok(json!({"status":"success","summary":value,"next_actions":[],"artifacts":[]}))
}

fn start(broker: &ToolBroker, name: &str, args: &Value, access: builtin::Access) -> Result<Value> {
    ensure!(
        !cfg!(windows),
        "Commands are not supported on Windows yet; nothing was run"
    );
    let jobs = broker.session.shell_jobs.clone();
    let session = broker.request.session_id;
    let root = broker.request.workspace.clone();
    let data = broker
        .artifacts
        .parent()
        .context("Application data directory missing")?
        .to_path_buf();
    let cwd = jobs.cwd(session, &root, &data)?;
    let background = args
        .get("run_in_background")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let marker = format!("__XYLO_PWD_{}__", Uuid::new_v4().simple());
    let (program, argv, label) = if name == "shell_command" {
        let command = builtin::text(args, "command")?;
        ensure!(!command.trim().is_empty(), "command is empty");
        let (program, argv) = code::shell_invocation(&wrapped_command(command, &marker), &cwd)?;
        (program, argv, command.to_owned())
    } else {
        let arguments: Vec<String> = args
            .get("args")
            .and_then(Value::as_array)
            .context("args must be an array")?
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .context("Each argument must be text")
            })
            .collect::<Result<_>>()?;
        let name = builtin::text(args, "program")?;
        (
            code::resolve_program(name, &cwd)?,
            arguments,
            name.to_owned(),
        )
    };
    let timeout = args
        .get("timeout_seconds")
        .and_then(Value::as_u64)
        .map(|seconds| Duration::from_secs(seconds.clamp(1, 1200)))
        .or_else(|| (!background).then_some(Duration::from_secs(120)));
    let network = if access.network {
        Network::Unrestricted
    } else {
        Network::None
    };
    let mut prepared = sandbox::prepare_coding_with_network(
        &program.to_string_lossy(),
        &argv,
        &root,
        &[],
        std::slice::from_ref(&data),
        network,
    )
    .map_err(|e| xylo_runtime::process::NotStarted(format!("{e:#}")))?;
    prepared.command.env("XYLO_SANDBOX_CWD", &cwd);
    if args.get("allow_local_binding").and_then(Value::as_bool) == Some(true) {
        ensure!(
            access.network,
            "Local listening requires network: true and a justification"
        );
        prepared.command.env("XYLO_SANDBOX_LOCAL_BINDING", "1");
    }
    let id = Uuid::new_v4();
    let directory = broker
        .artifacts
        .join("shell-jobs")
        .join(session.to_string());
    std::fs::create_dir_all(&directory)?;
    let log_path = directory.join(format!("{id}.log"));
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let log = options.open(&log_path)?;
    let job = Arc::new(Job {
        session,
        background,
        cancel: if background {
            CancellationToken::new()
        } else {
            broker.cancel.child_token()
        },
        changed: Notify::new(),
        finished: AtomicBool::new(false),
        state: Mutex::new(JobState {
            snapshot: ShellJob {
                id,
                turn_id: broker.request.turn_id,
                command: label,
                cwd: cwd.clone(),
                log_path: log_path.clone(),
                running: true,
                exit_code: None,
                output: String::new(),
                error: None,
            },
            output: OutputBuffer::default(),
            log,
            capture: (name == "shell_command").then(|| CwdCapture::new(marker.clone())),
            update_pending: false,
            network_hint: None,
        }),
    });
    {
        let mut state = jobs.state.lock();
        if state
            .jobs
            .iter()
            .filter(|job| job.session == session && job.state.lock().snapshot.running)
            .count()
            >= MAX_JOBS
        {
            return Err(code::unapplied(
                "This conversation already has 8 running shell jobs; stop or wait for one first",
            ));
        }
        while state.jobs.len() >= RETAINED_JOBS {
            let Some(index) = state
                .jobs
                .iter()
                .position(|job| !job.state.lock().snapshot.running)
            else {
                break;
            };
            state.jobs.remove(index);
        }
        state.jobs.push_back(job.clone());
    }
    jobs.publish(&job);
    let observer = {
        let jobs = jobs.clone();
        let job = job.clone();
        Arc::new(move |stderr: bool, bytes: &[u8]| jobs.append(&job, stderr, bytes))
            as xylo_runtime::process::OutputObserver
    };
    let task_job = job.clone();
    let task_jobs = jobs.clone();
    let run = async move {
        let sandbox::PreparedSandbox {
            command,
            temporary,
            status_file,
        } = prepared;
        let mut result = xylo_runtime::process::run_prepared_observed(
            command,
            &cwd,
            timeout,
            task_job.cancel.clone(),
            Some(observer),
            true,
        )
        .await;
        if let Some(failure) = sandbox::read_start_status(&status_file) {
            result = Err(xylo_runtime::process::NotStarted(failure.to_string()).into());
        }
        let denied = sandbox::read_network_denials(&status_file);
        if !denied.is_empty() {
            let hint = format!(
                "The sandbox blocked internet access to {}. Run again with network: true and a justification if the command needs the network.",
                denied.join(", ")
            );
            task_job.state.lock().network_hint = Some(hint.clone());
            task_jobs.append(&task_job, false, format!("\n{hint}\n").as_bytes());
        }
        task_jobs.finish(&task_job, &result, &root, &data);
        drop(temporary);
        result
    };
    if background {
        broker.runtime.spawn(run);
        Ok(
            json!({"status":"success","summary":{"job_id":id,"running":true,"log_path":log_path},"next_actions":["Use shell_output to wait for output or shell_kill to stop this job"],"artifacts":[]}),
        )
    } else {
        let result = broker.runtime.block_on(run)?;
        let mut capture = CwdCapture::new(marker);
        let mut errors = capture.push(result.stderr.as_bytes());
        errors.extend(capture.finish().0);
        let stderr = String::from_utf8_lossy(&errors);
        let status = if result.code == Some(0) {
            "success"
        } else {
            "failed"
        };
        let mut value = json!({"status":status,"exit_code":result.code,"summary":{"exit_code":result.code,"stdout":result.stdout,"stderr":stderr,"output_truncated":result.truncated,"log_path":log_path},"next_actions":[],"artifacts":[]});
        if let Some(hint) = &job.state.lock().network_hint {
            value["summary"]["network_hint"] = json!(hint);
        }
        Ok(value)
    }
}

#[cfg(test)]
mod tests;
