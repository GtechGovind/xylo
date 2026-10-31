use super::*;
const MIB: u64 = 1024 * 1024;
fn hardware() -> ResourceSnapshot {
    ResourceSnapshot {
        total_memory_bytes: 8 * 1024 * MIB,
        available_memory_bytes: 2 * 1024 * MIB,
        cpu_threads: 8,
        cpu_busy_percent: 0.,
    }
}
fn directory() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().canonicalize().unwrap().join("admission");
    (temp, path)
}
/// One attempt without a place in line; a queued result drops its ticket.
fn admit(
    directory: &Path,
    demand: TaskDemand,
    hardware: ResourceSnapshot,
    cancel: &CancellationToken,
) -> Result<Option<SharedResourceLease>> {
    Ok(
        match try_acquire(directory, demand, hardware, None, cancel)? {
            Attempt::Admitted(lease) => Some(lease),
            Attempt::Queued(_) => None,
        },
    )
}
fn demand() -> TaskDemand {
    TaskDemand {
        memory_bytes: 1024 * MIB,
        cpu_share: 1.,
    }
}

#[cfg(windows)]
#[test]
fn canonical_windows_directory_admission_creates_a_live_reservation() {
    let (_temp, canonical) = directory();
    let ordinary = PathBuf::from(
        canonical
            .to_str()
            .unwrap()
            .strip_prefix(r"\\?\")
            .expect("The native canonical temp path must use a verbatim drive prefix"),
    );
    for path in [canonical.join("verbatim-雪"), ordinary.join("ordinary-雪")] {
        let cancel = CancellationToken::new();
        let lease = admit(&path, demand(), hardware(), &cancel)
            .unwrap_or_else(|error| panic!("Admission for {path:?} failed: {error:#}"))
            .expect("A deterministic available-memory budget must admit the reservation");
        assert_eq!(lease.directory(), path.canonicalize().unwrap());
        assert_eq!(
            reap_and_sum(lease.directory(), &cancel).unwrap().totals(),
            (1, 1024 * MIB, 1.)
        );
        drop(lease);
        assert_eq!(reap_and_sum(&path, &cancel).unwrap().totals(), (0, 0, 0.));
    }
}

#[test]
fn independent_handles_share_accounting_and_drop_is_reaped_by_os_liveness() {
    let (_temp, path) = directory();
    let cancel = CancellationToken::new();
    let first = admit(&path, demand(), hardware(), &cancel)
        .unwrap()
        .unwrap();
    assert!(
        admit(&path, demand(), hardware(), &cancel)
            .unwrap()
            .is_none()
    );
    let record = first.directory().join(format!("{}.json", first.id()));
    assert!(record.exists());
    drop(first);
    let second = admit(&path, demand(), hardware(), &cancel)
        .unwrap()
        .unwrap();
    assert!(!record.exists());
    assert_eq!(
        reap_and_sum(&path, &cancel).unwrap().totals(),
        (1, 1024 * MIB, 1.)
    );
    drop(second);
    assert_eq!(reap_and_sum(&path, &cancel).unwrap().totals(), (0, 0, 0.));
}

#[test]
fn cpu_and_available_memory_changes_control_admission_without_profiles() {
    let (_temp, path) = directory();
    let cancel = CancellationToken::new();
    let mut busy = hardware();
    busy.cpu_threads = 2;
    busy.cpu_busy_percent = 100.;
    let first = admit(
        &path,
        TaskDemand {
            memory_bytes: MIB,
            cpu_share: 1.,
        },
        busy,
        &cancel,
    )
    .unwrap()
    .unwrap();
    assert!(
        admit(
            &path,
            TaskDemand {
                memory_bytes: MIB,
                cpu_share: 1.
            },
            busy,
            &cancel
        )
        .unwrap()
        .is_none()
    );
    drop(first);
    let mut low = hardware();
    low.available_memory_bytes = 64 * MIB;
    assert!(admit(&path, demand(), low, &cancel).unwrap().is_none());
    assert!(
        admit(&path, demand(), hardware(), &cancel)
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn queued_cancellation_leaves_no_live_reservation() {
    let (_temp, path) = directory();
    let cancel = CancellationToken::new();
    let caller = cancel.clone();
    let queued_path = path.clone();
    let task = tokio::spawn(async move {
        acquire(
            queued_path,
            TaskDemand {
                memory_bytes: u64::MAX,
                cpu_share: 1.,
            },
            caller,
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(30)).await;
    cancel.cancel();
    assert!(
        tokio::time::timeout(Duration::from_millis(500), task)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    let check_path = path.clone();
    assert_eq!(
        tokio::task::spawn_blocking(move || {
            let canonical = prepare_directory(&check_path, &CancellationToken::new()).unwrap();
            let _lock = metadata_lock(&canonical, &CancellationToken::new()).unwrap();
            reap_and_sum(&canonical, &CancellationToken::new())
                .unwrap()
                .totals()
        })
        .await
        .unwrap(),
        (0, 0, 0.)
    );
}

#[tokio::test]
async fn cancelled_outer_future_retains_admission_until_blocking_work_really_finishes() {
    let (_temp, path) = directory();
    let lease = std::sync::Arc::new(
        admit(&path, demand(), hardware(), &CancellationToken::new())
            .unwrap()
            .unwrap(),
    );
    let (entered, waiting) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let (finished, done) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(crate::scope_shared(lease.clone(), async move {
        crate::spawn_blocking(move || {
            let _ = entered.send(());
            released.recv_timeout(Duration::from_secs(3)).unwrap();
            let _ = finished.send(());
        })
        .await
        .unwrap();
    }));
    drop(lease);
    tokio::time::timeout(Duration::from_secs(1), waiting)
        .await
        .unwrap()
        .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    // The async task is gone, but its actual native closure still owns the
    // shared lease. A second worker cannot spend that reserved headroom.
    assert!(
        admit(&path, demand(), hardware(), &CancellationToken::new())
            .unwrap()
            .is_none()
    );
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(1), done)
        .await
        .unwrap()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        if admit(&path, demand(), hardware(), &CancellationToken::new())
            .unwrap()
            .is_some()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Blocking work leaked its admission lease"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[test]
fn live_corrupt_metadata_fails_closed_but_dead_corrupt_record_is_reclaimed() {
    let (_temp, path) = directory();
    let cancel = CancellationToken::new();
    let lease = admit(&path, demand(), hardware(), &cancel)
        .unwrap()
        .unwrap();
    let record = path.join(format!("{}.json", lease.id()));
    std::fs::write(&record, b"{invalid").unwrap();
    assert!(
        admit(&path, demand(), hardware(), &cancel)
            .unwrap_err()
            .to_string()
            .contains("corrupt")
    );
    drop(lease);
    assert!(
        admit(&path, demand(), hardware(), &cancel)
            .unwrap()
            .is_some()
    );
    assert!(!record.exists());
}

#[test]
fn traversal_unexpected_files_and_oversized_records_are_rejected() {
    let (temp, path) = directory();
    let cancel = CancellationToken::new();
    // PathBuf::join normalizes parent components for Windows verbatim
    // paths. Preserve the actual request bytes to exercise rejection.
    let mut raw = temp.path().canonicalize().unwrap().into_os_string();
    for part in ["admission", "..", "outside"] {
        raw.push(std::path::MAIN_SEPARATOR_STR);
        raw.push(part);
    }
    let traversal = PathBuf::from(raw);
    assert!(
        traversal
            .components()
            .any(|part| matches!(part, Component::ParentDir)),
        "The fixture must reach admission with an actual parent component: {traversal:?}"
    );
    let error = admit(&traversal, demand(), hardware(), &cancel).unwrap_err();
    assert!(error.to_string().contains("without traversal"), "{error:#}");
    assert!(!temp.path().join("outside").exists());
    prepare_directory(&path, &cancel).unwrap();
    std::fs::write(path.join("credentials.json"), b"{}").unwrap();
    assert!(admit(&path, demand(), hardware(), &cancel).is_err());
    std::fs::remove_file(path.join("credentials.json")).unwrap();
    std::fs::write(path.join(format!("{}.json", Uuid::new_v4())), vec![0; 1025]).unwrap();
    assert!(admit(&path, demand(), hardware(), &cancel).is_err());
}

#[cfg(unix)]
#[test]
fn directory_and_metadata_symlinks_never_touch_the_outside_target() {
    let (temp, path) = directory();
    let cancel = CancellationToken::new();
    let outside = temp.path().canonicalize().unwrap().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, &path).unwrap();
    assert!(admit(&path, demand(), hardware(), &cancel).is_err());
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
    std::fs::remove_file(&path).unwrap();
    prepare_directory(&path, &cancel).unwrap();
    let target = outside.join("private");
    std::fs::write(&target, b"unchanged").unwrap();
    std::os::unix::fs::symlink(&target, path.join(METADATA_LOCK)).unwrap();
    assert!(admit(&path, demand(), hardware(), &cancel).is_err());
    assert_eq!(std::fs::read(&target).unwrap(), b"unchanged");
}

#[test]
fn waiters_are_admitted_in_arrival_order() {
    let (_temp, path) = directory();
    let cancel = CancellationToken::new();
    let small = TaskDemand {
        memory_bytes: 64 * MIB,
        cpu_share: 0.25,
    };
    let running = admit(&path, demand(), hardware(), &cancel)
        .unwrap()
        .unwrap();
    // The large request queues first because the running one fills memory.
    let Attempt::Queued(large) = try_acquire(&path, demand(), hardware(), None, &cancel).unwrap()
    else {
        panic!("The large request must queue while memory is reserved");
    };
    // A later small request would fit, but it must not jump the queue.
    let Attempt::Queued(later) = try_acquire(&path, small, hardware(), None, &cancel).unwrap()
    else {
        panic!("A later request must wait behind an earlier waiter");
    };
    assert!(later.sequence > large.sequence);
    let Attempt::Queued(later) =
        try_acquire(&path, small, hardware(), Some(later), &cancel).unwrap()
    else {
        panic!("A waiter behind the head of the queue must keep waiting");
    };
    drop(running);
    let Attempt::Admitted(large_lease) =
        try_acquire(&path, demand(), hardware(), Some(large), &cancel).unwrap()
    else {
        panic!("The head of the queue must be admitted once it fits");
    };
    let Attempt::Admitted(small_lease) =
        try_acquire(&path, small, hardware(), Some(later), &cancel).unwrap()
    else {
        panic!("The next waiter is admitted after the head");
    };
    let ledger = reap_and_sum(&path, &cancel).unwrap();
    assert!(ledger.waiters.is_empty());
    assert_eq!(ledger.count, 2);
    drop((large_lease, small_lease));
}

#[test]
fn abandoned_tickets_do_not_block_the_queue() {
    let (_temp, path) = directory();
    let cancel = CancellationToken::new();
    let running = admit(&path, demand(), hardware(), &cancel)
        .unwrap()
        .unwrap();
    let Attempt::Queued(abandoned) =
        try_acquire(&path, demand(), hardware(), None, &cancel).unwrap()
    else {
        panic!("The request must queue");
    };
    drop(abandoned);
    drop(running);
    assert!(
        admit(&path, demand(), hardware(), &cancel)
            .unwrap()
            .is_some()
    );
    assert!(
        std::fs::read_dir(&path)
            .unwrap()
            .filter_map(Result::ok)
            .all(|entry| !entry.file_name().to_string_lossy().contains(".queue"))
    );
}

#[test]
fn a_waiter_that_cannot_fit_even_when_idle_holds_nobody_back() {
    let (_temp, path) = directory();
    let cancel = CancellationToken::new();
    let small = TaskDemand {
        memory_bytes: 64 * MIB,
        cpu_share: 0.25,
    };
    let huge = TaskDemand {
        memory_bytes: 6 * 1024 * MIB,
        cpu_share: 1.,
    };
    let running = admit(&path, demand(), hardware(), &cancel)
        .unwrap()
        .unwrap();
    // Even with the running reservation gone, other programs leave too little
    // memory for the huge request.
    let Attempt::Queued(head) = try_acquire(&path, huge, hardware(), None, &cancel).unwrap() else {
        panic!("The huge request must queue");
    };
    let small_lease = admit(&path, small, hardware(), &cancel)
        .unwrap()
        .expect("a request that fits runs past a head that cannot start");
    // The head keeps its place and is admitted once memory frees up.
    drop((running, small_lease));
    let roomy = ResourceSnapshot {
        available_memory_bytes: 7 * 1024 * MIB,
        ..hardware()
    };
    let Attempt::Admitted(head_lease) =
        try_acquire(&path, huge, roomy, Some(head), &cancel).unwrap()
    else {
        panic!("The head is admitted once it fits");
    };
    drop(head_lease);
}

#[test]
fn a_waiter_that_could_start_when_reservations_end_still_holds_its_place() {
    let (_temp, path) = directory();
    let cancel = CancellationToken::new();
    let small = TaskDemand {
        memory_bytes: 64 * MIB,
        cpu_share: 0.25,
    };
    let _running = admit(&path, demand(), hardware(), &cancel)
        .unwrap()
        .unwrap();
    let Attempt::Queued(_head) = try_acquire(&path, demand(), hardware(), None, &cancel).unwrap()
    else {
        panic!("The second large request must queue");
    };
    assert!(
        admit(&path, small, hardware(), &cancel).unwrap().is_none(),
        "the head only waits for Xylo's own reservation, so nobody jumps it"
    );
}
