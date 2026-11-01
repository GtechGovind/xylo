use super::*;
use xylo_core::PermissionMode;

#[tokio::test]
async fn wakeup_one_shot_survives_restart_and_is_claimed_once() {
    let (_root, scheduler) = fixture();
    let session = Uuid::new_v4();
    let task = scheduler
        .create_wakeup(
            session,
            PermissionMode::AskForApproval,
            60,
            "Check status".into(),
            "Waiting for build".into(),
            None,
        )
        .await
        .unwrap();
    set_due(&scheduler, task.id, Utc::now() - Duration::seconds(1)).await;
    let reopened = Scheduler::new(scheduler.path.clone(), scheduler.workspace.clone()).unwrap();
    let (a, b) = tokio::join!(scheduler.claim_due(), reopened.claim_due());
    let claims: Vec<_> = a.unwrap().into_iter().chain(b.unwrap()).collect();
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0].wakeup.as_ref().unwrap().session_id, session);
    reopened.complete(task.id).await.unwrap();
    reopened.complete(task.id).await.unwrap();
    assert!(!stored(&scheduler, task.id).await.enabled);
    assert!(scheduler.claim_due().await.unwrap().is_empty());
}

#[tokio::test]
async fn wakeup_expired_claim_is_not_repeated_and_loop_misses_coalesce() {
    let (_root, scheduler) = fixture();
    let task = scheduler
        .create_wakeup(
            Uuid::new_v4(),
            PermissionMode::ReadOnly,
            60,
            "Check".into(),
            "Monitor".into(),
            Some(60),
        )
        .await
        .unwrap();
    let due = Utc::now() - Duration::minutes(20);
    set_due(&scheduler, task.id, due).await;
    scheduler.claim_due().await.unwrap();
    scheduler
        .transact(true, move |tasks| {
            tasks[0].claim.as_mut().unwrap().lease_until = Utc::now() - Duration::seconds(1);
            Ok(())
        })
        .await
        .unwrap();
    assert!(scheduler.claim_due().await.unwrap().is_empty());
    let saved = stored(&scheduler, task.id).await;
    assert!(saved.interrupted.is_some());
    assert!(saved.next_run > Utc::now());
    scheduler.pause(task.id).await.unwrap();
    set_due(&scheduler, task.id, Utc::now() - Duration::seconds(1)).await;
    assert!(scheduler.claim_due().await.unwrap().is_empty());
}

#[tokio::test]
async fn wakeup_validates_bounds_and_does_not_allow_authority_replacement() {
    let (_root, scheduler) = fixture();
    for delay in [0, 59, 3601, u64::MAX] {
        assert!(
            scheduler
                .create_wakeup(
                    Uuid::new_v4(),
                    PermissionMode::AskForApproval,
                    delay,
                    "p".into(),
                    "r".into(),
                    None
                )
                .await
                .is_err()
        );
    }
    let mut task = scheduler
        .create_wakeup(
            Uuid::new_v4(),
            PermissionMode::AskForApproval,
            60,
            "p".into(),
            "r".into(),
            None,
        )
        .await
        .unwrap();
    task.wakeup.as_mut().unwrap().permissions = PermissionMode::FullAccess;
    assert!(scheduler.save(task).await.is_err());
}

#[test]
fn wakeup_loop_parser_and_permission_cap_are_bounded() {
    assert_eq!(
        parse_loop("/loop check build").unwrap(),
        (300, "check build".into())
    );
    assert_eq!(parse_loop("/loop 1h check build").unwrap().0, 3600);
    for invalid in [
        "/loop",
        "/loop 1s hi",
        "/loop 61m hi",
        "/looping hi",
        "/loop 5m",
    ] {
        assert!(parse_loop(invalid).is_err(), "{invalid}");
    }
    for mode in PermissionMode::ALL {
        assert_ne!(
            wakeup_mode(PermissionMode::FullAccess, mode),
            PermissionMode::FullAccess
        );
        assert!(!wakeup_mode(PermissionMode::ReadOnly, mode).allows_writes());
        assert_eq!(
            wakeup_mode(PermissionMode::Plan, mode),
            PermissionMode::Plan
        );
    }
}
