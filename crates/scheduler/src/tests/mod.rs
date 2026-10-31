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

async fn set_due(scheduler: &Scheduler, id: Uuid, due: DateTime<Utc>) {
    scheduler
        .transact(true, move |tasks| {
            tasks
                .iter_mut()
                .find(|task| task.id == id)
                .unwrap()
                .next_run = due;
            Ok(())
        })
        .await
        .unwrap();
}

async fn stored(scheduler: &Scheduler, id: Uuid) -> ScheduledTask {
    scheduler
        .list()
        .await
        .unwrap()
        .into_iter()
        .find(|task| task.id == id)
        .unwrap()
}

fn fixture() -> (tempfile::TempDir, Scheduler) {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().canonicalize().unwrap();
    let scheduler = Scheduler::new(workspace.join("schedules.json"), workspace).unwrap();
    (root, scheduler)
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
    set_due(&first, a.id, Utc::now() - Duration::seconds(1)).await;
    let (claims_a, claims_b) = tokio::join!(first.claim_due(), second.claim_due());
    let claims: Vec<_> = claims_a
        .unwrap()
        .into_iter()
        .chain(claims_b.unwrap())
        .collect();
    assert_eq!(claims.len(), 1);
    let claim = claims[0].claim.unwrap();
    assert_eq!(
        claim.occurrence,
        ScheduledTask::occurrence_at(a.id, claims[0].next_run)
    );
    first.renew(a.id).await.unwrap();
    assert!(second.claim_due().await.unwrap().is_empty());
    first.release(a.id).await.unwrap();
    let retried = second.claim_due().await.unwrap();
    assert_eq!(retried[0].claim.unwrap().occurrence, claim.occurrence);
    second.complete(a.id).await.unwrap();
    let completed = stored(&first, a.id).await;
    assert!(completed.next_run > Utc::now() && completed.claim.is_none());
    first.remove(b.id).await.unwrap();
    assert_eq!(second.list().await.unwrap().len(), 1);
    let restored = Scheduler::new(path, workspace).unwrap();
    assert_eq!(restored.list().await.unwrap().len(), 1);
}

#[tokio::test]
async fn an_expired_claim_is_never_dispatched_again() {
    let (_root, scheduler) = fixture();
    let task = scheduler
        .create("Daily".into(), "0 0 9 * * * *".into())
        .await
        .unwrap();
    let due = Utc::now() - Duration::minutes(1);
    set_due(&scheduler, task.id, due).await;
    let claimed = scheduler.claim_due().await.unwrap();
    let occurrence = claimed[0].claim.unwrap().occurrence;
    scheduler
        .transact(true, move |tasks| {
            let claim = tasks[0].claim.as_mut().unwrap();
            claim.lease_until = Utc::now() - Duration::seconds(1);
            Ok(())
        })
        .await
        .unwrap();
    let reclaimed = scheduler.claim_due().await.unwrap();
    assert!(
        reclaimed
            .iter()
            .all(|task| task.claim.unwrap().occurrence != occurrence),
        "A host that died mid-run must not cause the same occurrence to run twice"
    );
    let after = stored(&scheduler, task.id).await;
    assert_eq!(after.interrupted, Some(occurrence));
    assert!(after.next_run > due);
}

#[tokio::test]
async fn completion_is_idempotent_and_follows_the_schedule() {
    let (_root, scheduler) = fixture();
    let task = scheduler
        .create("Hourly".into(), "0 0 * * * * *".into())
        .await
        .unwrap();
    let schedule = cron::Schedule::from_str("0 0 * * * * *").unwrap();
    let next_hour = schedule.upcoming(Utc).next().unwrap();
    let due = next_hour - Duration::hours(3);
    set_due(&scheduler, task.id, due).await;
    assert_eq!(scheduler.claim_due().await.unwrap().len(), 1);
    scheduler.complete(task.id).await.unwrap();
    let catch_up = stored(&scheduler, task.id).await.next_run;
    assert_eq!(
        catch_up,
        next_hour - Duration::hours(1),
        "Missed occurrences coalesce into the most recent one"
    );
    scheduler.complete(task.id).await.unwrap();
    assert_eq!(
        stored(&scheduler, task.id).await.next_run,
        catch_up,
        "A repeated acknowledgement must not skip an occurrence"
    );
    assert_eq!(scheduler.claim_due().await.unwrap().len(), 1);
    scheduler.complete(task.id).await.unwrap();
    assert_eq!(stored(&scheduler, task.id).await.next_run, next_hour);
}

#[tokio::test]
async fn saving_a_stale_copy_keeps_the_running_claim() {
    let (_root, scheduler) = fixture();
    let task = scheduler
        .create("Task".into(), "0 * * * * * *".into())
        .await
        .unwrap();
    set_due(&scheduler, task.id, Utc::now() - Duration::seconds(1)).await;
    let claim = scheduler.claim_due().await.unwrap()[0].claim;
    let mut edited = task.clone();
    edited.prompt = "Edited".into();
    let saved = scheduler.save(edited).await.unwrap();
    assert_eq!(saved.claim, claim);
    assert_eq!(saved.prompt, "Edited");
    assert!(scheduler.claim_due().await.unwrap().is_empty());
}

#[tokio::test]
async fn an_unreadable_store_is_replaced() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().canonicalize().unwrap();
    let path = workspace.join("schedules.json");
    std::fs::write(&path, r#"[{"id":"not-a-task"}]"#).unwrap();
    let scheduler = Scheduler::new(path.clone(), workspace.clone()).unwrap();
    assert!(scheduler.list().await.unwrap().is_empty());
    assert!(!path.exists(), "The unreadable store is deleted");
    assert_eq!(
        std::fs::read_dir(&workspace)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name() != "schedules.lock")
            .count(),
        0,
        "No copy of the old store is kept"
    );
    scheduler
        .create("Task".into(), "0 * * * * * *".into())
        .await
        .unwrap();
    assert_eq!(scheduler.list().await.unwrap().len(), 1);
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
