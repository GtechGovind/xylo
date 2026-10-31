//! Cross-process durable UTC scheduling, independent of native capabilities.
//! The host claims prompts, renews active leases, and acknowledges completion;
//! this library never executes an agent or shell command.
//!
//! Each due time is one occurrence with a stable id (`ScheduleClaim::occurrence`,
//! derived from the task id and due time). Dispatch is at most once per
//! occurrence: a claim whose lease expires (its host died) is never handed out
//! again; the task moves on to its next scheduled time and records the
//! occurrence in `interrupted`. Completion is idempotent and advances the task
//! from the claimed due time, so the cadence follows the schedule rather than
//! how long a run took; occurrences missed while a run or the app was busy are
//! coalesced into one catch-up run.

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::PathBuf,
    str::FromStr,
};
use uuid::Uuid;
use xylo_core::{ToolDefinition, ToolObservation};

const MAX_TASKS: usize = 128;
const MAX_STORE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_PROMPT_BYTES: usize = 8192;
const LEASE_MINUTES: i64 = 5;
/// Bound on the scan for the latest missed occurrence (an every-second cron
/// that was offline for days would otherwise iterate for a long time).
const MAX_MISSED_SCAN: usize = 100_000;

/// A durable recurring prompt.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ScheduledTask {
    pub id: Uuid,
    pub prompt: String,
    pub workspace: PathBuf,
    /// Seven-field UTC cron expression.
    pub cron: String,
    /// Due time of the next occurrence to dispatch.
    pub next_run: DateTime<Utc>,
    pub enabled: bool,
    /// The occurrence a host is currently running, if any. Owned by the
    /// scheduler: [`Scheduler::save`] never takes it from the caller.
    pub claim: Option<ScheduleClaim>,
    /// The most recent occurrence whose host stopped renewing its lease
    /// before acknowledging it. It was not dispatched again.
    pub interrupted: Option<Uuid>,
}

/// One claimed occurrence of a task.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ScheduleClaim {
    /// Stable id of this occurrence; hosts use it as the session and turn id.
    pub occurrence: Uuid,
    /// The scheduled time this occurrence stands for.
    pub due: DateTime<Utc>,
    pub lease_until: DateTime<Utc>,
}

impl ScheduledTask {
    /// The stable id of the occurrence due at `due`.
    pub fn occurrence_at(id: Uuid, due: DateTime<Utc>) -> Uuid {
        Uuid::new_v5(&id, due.to_rfc3339().as_bytes())
    }
}

/// The occurrence after `due`. When that time has already passed (a run
/// overlapped later occurrences or the app was not running), the most recent
/// missed occurrence is returned so they coalesce into one catch-up run.
fn following(
    schedule: &cron::Schedule,
    due: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let mut upcoming = schedule.after(&due);
    let first = upcoming.next()?;
    if first > now {
        return Some(first);
    }
    let mut latest = first;
    for (scanned, occurrence) in upcoming.enumerate() {
        if occurrence > now {
            return Some(latest);
        }
        if scanned >= MAX_MISSED_SCAN {
            return Some(now);
        }
        latest = occurrence;
    }
    Some(latest)
}

/// No cached state: every operation locks and rereads the durable store.
#[derive(Clone)]
pub struct Scheduler {
    path: PathBuf,
    workspace: PathBuf,
}

impl Scheduler {
    /// Open the store at `path`; new schedules default to `workspace`. A store
    /// that does not decode or validate is deleted and replaced by an empty
    /// one; schedules are not carried across format changes.
    pub fn new(path: PathBuf, workspace: PathBuf) -> Result<Self> {
        ensure!(path.is_absolute(), "Schedule store path must be absolute");
        std::fs::create_dir_all(path.parent().context("Schedule path has no parent")?)?;
        let workspace = std::fs::canonicalize(workspace)?;
        ensure!(workspace.is_dir(), "Schedule workspace must be a directory");
        let scheduler = Self { path, workspace };
        scheduler.transaction(false, |_| Ok(()))?;
        Ok(scheduler)
    }

    fn validate_task(task: &ScheduledTask) -> Result<()> {
        ensure!(
            !task.prompt.trim().is_empty() && task.prompt.len() <= MAX_PROMPT_BYTES,
            "Scheduled prompt must contain 1..8192 bytes"
        );
        ensure!(
            task.workspace.is_absolute(),
            "Schedule must retain an absolute workspace"
        );
        ensure!(task.cron.len() <= 512, "Cron expression exceeds limit");
        cron::Schedule::from_str(&task.cron).context("Invalid UTC cron expression")?;
        Ok(())
    }

    fn read(&self) -> Result<Vec<ScheduledTask>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let file = File::open(&self.path)?;
        ensure!(
            file.metadata()?.len() <= MAX_STORE_BYTES,
            "Schedule store exceeds 2MiB budget"
        );
        let mut bytes = Vec::new();
        file.take(MAX_STORE_BYTES + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= MAX_STORE_BYTES,
            "Schedule store exceeds 2MiB budget"
        );
        let decode = || -> Result<Vec<ScheduledTask>> {
            let tasks: Vec<ScheduledTask> = serde_json::from_slice(&bytes)?;
            ensure!(tasks.len() <= MAX_TASKS, "Schedule count exceeds limit");
            let mut seen = std::collections::HashSet::new();
            for task in &tasks {
                Self::validate_task(task)?;
                ensure!(seen.insert(task.id), "Duplicate schedule id");
            }
            Ok(tasks)
        };
        decode().map_err(|error| InvalidStore(format!("{error:#}")).into())
    }

    fn write(&self, tasks: &[ScheduledTask]) -> Result<()> {
        ensure!(tasks.len() <= MAX_TASKS, "Schedule count exceeds limit");
        let bytes = serde_json::to_vec_pretty(tasks)?;
        ensure!(
            bytes.len() as u64 <= MAX_STORE_BYTES,
            "Schedule store exceeds 2MiB budget"
        );
        let temporary = self.path.with_extension(format!("{}.tmp", Uuid::new_v4()));
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            std::fs::rename(&temporary, &self.path)?;
            #[cfg(unix)]
            File::open(self.path.parent().unwrap())?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(temporary);
        }
        result
    }

    fn transaction<T>(
        &self,
        write: bool,
        f: impl FnOnce(&mut Vec<ScheduledTask>) -> Result<T>,
    ) -> Result<T> {
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.path.with_extension("lock"))?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match fs2::FileExt::try_lock_exclusive(&lock) {
                Ok(()) => break,
                Err(error)
                    if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() =>
                {
                    ensure!(
                        std::time::Instant::now() < deadline,
                        "Schedule store is busy; lock deadline exceeded"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(error) => return Err(error.into()),
            }
        }
        // An unreadable store is deleted under the lock and starts empty.
        let mut tasks = match self.read() {
            Ok(tasks) => tasks,
            Err(error) if error.downcast_ref::<InvalidStore>().is_some() => {
                std::fs::remove_file(&self.path)?;
                Vec::new()
            }
            Err(error) => return Err(error),
        };
        let previous = write.then(|| tasks.clone());
        let result = f(&mut tasks)?;
        if previous.as_ref().is_some_and(|previous| previous != &tasks) {
            self.write(&tasks)?;
        }
        // Closing the descriptor releases the lock on success and error paths.
        drop(lock);
        Ok(result)
    }

    async fn transact<T: Send + 'static>(
        &self,
        write: bool,
        f: impl FnOnce(&mut Vec<ScheduledTask>) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let scheduler = self.clone();
        tokio::task::spawn_blocking(move || scheduler.transaction(write, f)).await?
    }

    /// Every stored task.
    pub async fn list(&self) -> Result<Vec<ScheduledTask>> {
        self.transact(false, |tasks| Ok(tasks.clone())).await
    }

    /// Insert or update a task's user-editable fields (prompt, workspace,
    /// cron, enabled). The active claim and interruption record always come
    /// from the store, so a stale copy cannot release a running occurrence.
    /// Changing the cron expression reschedules from now.
    pub async fn save(&self, mut task: ScheduledTask) -> Result<ScheduledTask> {
        Self::validate_task(&task)?;
        task.workspace = std::fs::canonicalize(&task.workspace)?;
        ensure!(
            task.workspace.is_dir(),
            "Schedule workspace is not a directory"
        );
        self.transact(true, move |tasks| {
            if let Some(stored) = tasks.iter_mut().find(|t| t.id == task.id) {
                if stored.cron != task.cron {
                    stored.next_run = first_occurrence(&task.cron)?;
                }
                stored.prompt = task.prompt;
                stored.workspace = task.workspace;
                stored.cron = task.cron;
                stored.enabled = task.enabled;
                Ok(stored.clone())
            } else {
                ensure!(tasks.len() < MAX_TASKS, "Schedule limit reached");
                task.claim = None;
                task.interrupted = None;
                tasks.push(task.clone());
                Ok(task)
            }
        })
        .await
    }

    /// Create an enabled task in this scheduler's workspace.
    pub async fn create(&self, prompt: String, expression: String) -> Result<ScheduledTask> {
        let task = ScheduledTask {
            id: Uuid::new_v4(),
            prompt,
            workspace: self.workspace.clone(),
            next_run: first_occurrence(&expression)?,
            cron: expression,
            enabled: true,
            claim: None,
            interrupted: None,
        };
        self.save(task).await
    }

    /// Delete a task.
    pub async fn remove(&self, id: Uuid) -> Result<()> {
        self.transact(true, move |tasks| {
            let count = tasks.len();
            tasks.retain(|t| t.id != id);
            ensure!(tasks.len() != count, "Schedule not found");
            Ok(())
        })
        .await
    }

    /// Claim every enabled task whose next occurrence is due and hand each
    /// out exactly once; the returned tasks carry their new `claim`. Claims
    /// whose lease expired are retired first (see the module docs).
    pub async fn claim_due(&self) -> Result<Vec<ScheduledTask>> {
        self.transact(true, |tasks| {
            let now = Utc::now();
            let mut due = Vec::new();
            for task in tasks.iter_mut() {
                if let Some(claim) = task.claim
                    && claim.lease_until <= now
                {
                    task.interrupted = Some(claim.occurrence);
                    task.claim = None;
                    advance(task, claim.due, now)?;
                }
                if task.enabled && task.claim.is_none() && task.next_run <= now {
                    task.claim = Some(ScheduleClaim {
                        occurrence: ScheduledTask::occurrence_at(task.id, task.next_run),
                        due: task.next_run,
                        lease_until: now + Duration::minutes(LEASE_MINUTES),
                    });
                    due.push(task.clone());
                }
            }
            Ok(due)
        })
        .await
    }

    /// Acknowledge the claimed occurrence and advance to the next one. Calling
    /// it again, or for a task with no claim, changes nothing.
    pub async fn complete(&self, id: Uuid) -> Result<()> {
        self.transact(true, move |tasks| {
            let task = tasks
                .iter_mut()
                .find(|t| t.id == id)
                .context("Schedule not found")?;
            if let Some(claim) = task.claim.take() {
                advance(task, claim.due, Utc::now())?;
            }
            Ok(())
        })
        .await
    }

    /// Extend the lease of the claimed occurrence while its run is active.
    pub async fn renew(&self, id: Uuid) -> Result<()> {
        self.transact(true, move |tasks| {
            let task = tasks
                .iter_mut()
                .find(|t| t.id == id)
                .context("Schedule not found")?;
            let claim = task
                .claim
                .as_mut()
                .context("Schedule has not been claimed")?;
            claim.lease_until = Utc::now() + Duration::minutes(LEASE_MINUTES);
            Ok(())
        })
        .await
    }

    /// Release a failed/cancelled attempt without advancing its due date, so
    /// the same occurrence is offered again.
    pub async fn release(&self, id: Uuid) -> Result<()> {
        self.transact(true, move |tasks| {
            let task = tasks
                .iter_mut()
                .find(|t| t.id == id)
                .context("Schedule not found")?;
            task.claim = None;
            Ok(())
        })
        .await
    }

    /// Stop retries after a failed or cancelled occurrence until the user
    /// explicitly resumes it. This does not repeat uncertain external writes.
    pub async fn pause(&self, id: Uuid) -> Result<()> {
        self.transact(true, move |tasks| {
            let task = tasks
                .iter_mut()
                .find(|task| task.id == id)
                .context("Schedule not found")?;
            task.enabled = false;
            task.claim = None;
            Ok(())
        })
        .await
    }

    /// Summary for capability status reports.
    pub async fn status(&self) -> Value {
        match self.list().await {
            Ok(tasks) => json!({"state":"available","count":tasks.len(),"timezone":"UTC"}),
            Err(error) => json!({"state":"failed","message":error.to_string()}),
        }
    }

    /// Run one of the agent schedule tools from [`definitions`].
    pub async fn execute(&self, name: &str, args: &Value) -> Result<ToolObservation> {
        let string = |key: &str| {
            args.get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| anyhow::anyhow!("Missing string: {key}"))
        };
        let result = match name {
            "schedule_list" => json!({"timezone":"UTC","tasks":self.list().await?}),
            "schedule_create" => json!(self.create(string("prompt")?, string("cron")?).await?),
            "schedule_delete" => {
                let id = Uuid::parse_str(&string("id")?)?;
                self.remove(id).await?;
                json!({"deleted":id})
            }
            _ => anyhow::bail!("Unknown schedule tool"),
        };
        Ok(ToolObservation {
            status: "completed".into(),
            summary: result.to_string(),
            next_actions: vec![],
            artifacts: vec![],
        })
    }
}

/// Unreadable schedule store contents (as opposed to an I/O failure).
#[derive(Debug)]
struct InvalidStore(String);
impl std::fmt::Display for InvalidStore {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(output, "Invalid schedule store: {}", self.0)
    }
}
impl std::error::Error for InvalidStore {}

fn first_occurrence(expression: &str) -> Result<DateTime<Utc>> {
    cron::Schedule::from_str(expression)
        .context("Invalid UTC cron expression")?
        .upcoming(Utc)
        .next()
        .context("No future schedule occurrence")
}

fn advance(task: &mut ScheduledTask, due: DateTime<Utc>, now: DateTime<Utc>) -> Result<()> {
    task.next_run = following(&cron::Schedule::from_str(&task.cron)?, due, now)
        .context("Schedule has no future occurrence")?;
    Ok(())
}

/// Agent-visible schedule tools. `schedule_create` and `schedule_delete` are
/// writes; creating a recurring prompt always needs the user's approval.
pub fn definitions() -> Vec<ToolDefinition> {
    let tool = |name: &str, description: &str, properties: Value, required: Vec<&str>, writes| {
        ToolDefinition {
            name: name.into(),
            description: description.into(),
            parameters: json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}),
            writes,
        }
    };
    vec![
        tool(
            "schedule_list",
            "List durable UTC task schedules.",
            json!({}),
            vec![],
            false,
        ),
        tool(
            "schedule_create",
            "Create a recurring task with its workspace and UTC cron. Needs the user's approval; the app dispatches each occurrence once via its normal agent policy.",
            json!({"cron":{"type":"string","description":"Seven-field UTC cron: sec min hour day month weekday year"},"prompt":{"type":"string","maxLength":8192}}),
            vec!["cron", "prompt"],
            true,
        ),
        tool(
            "schedule_delete",
            "Delete a task schedule.",
            json!({"id":{"type":"string"}}),
            vec!["id"],
            true,
        ),
    ]
}

#[cfg(test)]
mod tests;
