//! Cross-process durable UTC scheduling, independent of native capabilities.
//! The host claims prompts, renews active leases, and acknowledges completion;
//! this library never executes an agent or shell command.

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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScheduledTask {
    pub id: Uuid,
    pub prompt: String,
    pub workspace: PathBuf,
    pub cron: String,
    pub next_run: DateTime<Utc>,
    pub enabled: bool,
    pub lease_until: Option<DateTime<Utc>>,
}

/// No cached state: every operation locks and rereads the durable store.
#[derive(Clone)]
pub struct Scheduler {
    path: PathBuf,
    workspace: PathBuf,
}

impl Scheduler {
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
        let tasks: Vec<ScheduledTask> = serde_json::from_slice(&bytes)
            .context("Invalid schedule store; preserve it for recovery")?;
        ensure!(tasks.len() <= MAX_TASKS, "Schedule count exceeds limit");
        let mut seen = std::collections::HashSet::new();
        for task in &tasks {
            Self::validate_task(task)?;
            ensure!(seen.insert(task.id), "Duplicate schedule id");
        }
        Ok(tasks)
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
        let mut tasks = self.read()?;
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

    pub async fn list(&self) -> Result<Vec<ScheduledTask>> {
        self.transact(false, |tasks| Ok(tasks.clone())).await
    }

    pub async fn save(&self, mut task: ScheduledTask) -> Result<ScheduledTask> {
        Self::validate_task(&task)?;
        task.workspace = std::fs::canonicalize(&task.workspace)?;
        ensure!(
            task.workspace.is_dir(),
            "Schedule workspace is not a directory"
        );
        self.transact(true, move |tasks| {
            if let Some(index) = tasks.iter().position(|t| t.id == task.id) {
                tasks[index] = task.clone();
            } else {
                ensure!(tasks.len() < MAX_TASKS, "Schedule limit reached");
                tasks.push(task.clone());
            }
            Ok(task)
        })
        .await
    }

    pub async fn create(&self, prompt: String, expression: String) -> Result<ScheduledTask> {
        let cron = cron::Schedule::from_str(&expression).context("Invalid UTC cron expression")?;
        let task = ScheduledTask {
            id: Uuid::new_v4(),
            prompt,
            workspace: self.workspace.clone(),
            cron: expression,
            next_run: cron
                .upcoming(Utc)
                .next()
                .context("No future schedule occurrence")?,
            enabled: true,
            lease_until: None,
        };
        self.save(task).await
    }

    pub async fn remove(&self, id: Uuid) -> Result<()> {
        self.transact(true, move |tasks| {
            let count = tasks.len();
            tasks.retain(|t| t.id != id);
            ensure!(tasks.len() != count, "Schedule not found");
            Ok(())
        })
        .await
    }

    pub async fn claim_due(&self) -> Result<Vec<ScheduledTask>> {
        self.transact(true, |tasks| {
            let now = Utc::now();
            let mut due = Vec::new();
            for task in tasks.iter_mut() {
                if task.enabled
                    && task.next_run <= now
                    && task.lease_until.is_none_or(|lease| lease <= now)
                {
                    task.lease_until = Some(now + Duration::minutes(LEASE_MINUTES));
                    due.push(task.clone());
                }
            }
            Ok(due)
        })
        .await
    }

    pub async fn complete(&self, id: Uuid) -> Result<()> {
        self.transact(true, move |tasks| {
            let task = tasks
                .iter_mut()
                .find(|t| t.id == id)
                .context("Schedule not found")?;
            task.next_run = cron::Schedule::from_str(&task.cron)?
                .upcoming(Utc)
                .next()
                .context("Schedule has no future occurrence")?;
            task.lease_until = None;
            Ok(())
        })
        .await
    }

    pub async fn renew(&self, id: Uuid) -> Result<()> {
        self.transact(true, move |tasks| {
            let task = tasks
                .iter_mut()
                .find(|t| t.id == id)
                .context("Schedule not found")?;
            ensure!(task.lease_until.is_some(), "Schedule has not been claimed");
            task.lease_until = Some(Utc::now() + Duration::minutes(LEASE_MINUTES));
            Ok(())
        })
        .await
    }

    /// Release a failed/cancelled attempt without advancing its due date.
    pub async fn release(&self, id: Uuid) -> Result<()> {
        self.transact(true, move |tasks| {
            let task = tasks
                .iter_mut()
                .find(|t| t.id == id)
                .context("Schedule not found")?;
            task.lease_until = None;
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
            task.lease_until = None;
            Ok(())
        })
        .await
    }

    pub async fn status(&self) -> Value {
        match self.list().await {
            Ok(tasks) => json!({"state":"available","count":tasks.len(),"timezone":"UTC"}),
            Err(error) => json!({"state":"failed","message":error.to_string()}),
        }
    }

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
            "Create a task with its workspace and UTC cron. The app dispatches via its normal agent policy.",
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
mod tests {
    use super::*;

    /// Invoked in a fresh test executable by the process-race test below.
    #[test]
    fn cross_process_writer_fixture() {
        let Some(path) = std::env::var_os("XYLO_SCHEDULER_FIXTURE_FILE") else {
            return;
        };
        let path = PathBuf::from(path);
        let workspace = path.parent().unwrap().to_path_buf();
        let scheduler = Scheduler::new(path, workspace).unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime
            .block_on(scheduler.create(
                format!("Process {}", std::process::id()),
                "0 * * * * * *".into(),
            ))
            .unwrap();
    }

    #[tokio::test]
    async fn simultaneous_process_writers_do_not_lose_updates() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().canonicalize().unwrap();
        let path = workspace.join("schedules.json");
        let scheduler = Scheduler::new(path.clone(), workspace).unwrap();
        let executable = std::env::current_exe().unwrap();
        let spawn = || {
            std::process::Command::new(&executable)
                .args([
                    "--exact",
                    "tests::cross_process_writer_fixture",
                    "--nocapture",
                ])
                .env("XYLO_SCHEDULER_FIXTURE_FILE", &path)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap()
        };
        let mut first = spawn();
        let mut second = spawn();
        assert!(first.wait().unwrap().success());
        assert!(second.wait().unwrap().success());
        assert_eq!(scheduler.list().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn independent_instances_reread_and_claim_exclusively() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().canonicalize().unwrap();
        let path = workspace.join("schedules.json");
        let first = Scheduler::new(path.clone(), workspace.clone()).unwrap();
        let second = Scheduler::new(path.clone(), workspace.clone()).unwrap();
        let a = first
            .create("Task A".into(), "0 * * * * * *".into())
            .await
            .unwrap();
        let b = second
            .create("Task B".into(), "0 * * * * * *".into())
            .await
            .unwrap();
        assert_eq!(first.list().await.unwrap().len(), 2);
        let mut a = a;
        a.next_run = Utc::now() - Duration::seconds(1);
        first.save(a.clone()).await.unwrap();
        let (claims_a, claims_b) = tokio::join!(first.claim_due(), second.claim_due());
        assert_eq!(claims_a.unwrap().len() + claims_b.unwrap().len(), 1);
        first.renew(a.id).await.unwrap();
        assert!(second.claim_due().await.unwrap().is_empty());
        first.release(a.id).await.unwrap();
        assert_eq!(second.claim_due().await.unwrap().len(), 1);
        second.complete(a.id).await.unwrap();
        assert!(
            first
                .list()
                .await
                .unwrap()
                .iter()
                .find(|t| t.id == a.id)
                .unwrap()
                .next_run
                > Utc::now()
        );
        first.remove(b.id).await.unwrap();
        assert_eq!(second.list().await.unwrap().len(), 1);
        let restored = Scheduler::new(path, workspace).unwrap();
        assert_eq!(restored.list().await.unwrap().len(), 1);
    }
    #[tokio::test]
    async fn rejected_mutation_preserves_store_and_prompt_budget() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().canonicalize().unwrap();
        let path = workspace.join("schedules.json");
        let scheduler = Scheduler::new(path, workspace).unwrap();
        assert!(
            scheduler
                .create("x".repeat(MAX_PROMPT_BYTES + 1), "0 * * * * * *".into())
                .await
                .is_err()
        );
        assert!(scheduler.list().await.unwrap().is_empty());
        assert!(
            scheduler
                .create("Task".into(), "invalid".into())
                .await
                .is_err()
        );
    }
}
