use super::*;
use std::time::Duration;
use uuid::Uuid;
use xylo_core::ShellJob;

#[tokio::test]
async fn a_full_output_queue_cannot_drop_a_quiet_jobs_start() {
    let (output, mut receive) = mpsc::channel(1);
    let sequence = Arc::new(AtomicU64::new(0));
    let session = Uuid::new_v4();
    super::super::send(
        &output,
        &sequence,
        Uuid::nil(),
        Some(session),
        EngineEvent::Delta {
            text: "queued".into(),
        },
    )
    .await
    .unwrap();
    let events = sink(output, sequence);
    let job = ShellJob {
        id: Uuid::new_v4(),
        turn_id: Uuid::new_v4(),
        command: "quiet server".into(),
        cwd: "/project".into(),
        log_path: "/data/log".into(),
        running: true,
        exit_code: None,
        output: String::new(),
        error: None,
    };
    events(session, EngineEvent::ShellJobUpdated { job: job.clone() });
    receive.recv().await.unwrap();
    let received = tokio::time::timeout(Duration::from_millis(500), receive.recv())
        .await
        .expect("a quiet running job must be delivered even when the queue was full")
        .unwrap();
    assert!(
        matches!(received.payload, EngineEvent::ShellJobUpdated { job: observed } if observed.id == job.id && observed.running)
    );
    drop(events);
    assert!(
        tokio::time::timeout(Duration::from_millis(500), receive.recv())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn busy_output_coalesces_to_the_latest_job_state_and_drains_on_shutdown() {
    let (output, mut receive) = mpsc::channel(1);
    let sequence = Arc::new(AtomicU64::new(0));
    let session = Uuid::new_v4();
    super::super::send(
        &output,
        &sequence,
        Uuid::nil(),
        Some(session),
        EngineEvent::Delta {
            text: "queued".into(),
        },
    )
    .await
    .unwrap();
    let events = sink(output, sequence);
    let mut job = ShellJob {
        id: Uuid::new_v4(),
        turn_id: Uuid::new_v4(),
        command: "server".into(),
        cwd: "/project".into(),
        log_path: "/data/log".into(),
        running: true,
        exit_code: None,
        output: String::new(),
        error: None,
    };
    events(session, EngineEvent::ShellJobUpdated { job: job.clone() });
    tokio::task::yield_now().await;
    for index in 0..100 {
        job.output = index.to_string();
        events(session, EngineEvent::ShellJobUpdated { job: job.clone() });
    }
    job.running = false;
    job.exit_code = Some(0);
    events(session, EngineEvent::ShellJobUpdated { job: job.clone() });
    drop(events);
    let mut updates = Vec::new();
    while let Some(event) = tokio::time::timeout(Duration::from_millis(500), receive.recv())
        .await
        .unwrap()
    {
        if let EngineEvent::ShellJobUpdated { job } = event.payload {
            updates.push(job);
        }
    }
    assert!(updates.len() <= 2, "intermediate tails should coalesce");
    let last = updates.last().unwrap();
    assert!(!last.running);
    assert_eq!(last.exit_code, Some(0));
    assert_eq!(last.output, "99");
}
