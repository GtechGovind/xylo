use super::*;

#[test]
fn independent_execution_handles_report_contention_until_the_owner_drops() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("execution.lock");
    let owner = try_lock(open(&path).unwrap()).unwrap().unwrap();
    assert!(
        try_lock(open(&path).unwrap()).unwrap().is_none(),
        "A second handle must classify the live OS lock as contention"
    );
    drop(owner);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        if try_lock(open(&path).unwrap()).unwrap().is_some() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "Execution lease still has an OS owner after parent drop"
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

#[cfg(unix)]
#[test]
fn cloexec_fork_reference_keeps_authority_until_child_exit() {
    use std::{
        process::{Command, Stdio},
        time::{Duration, Instant},
    };
    const CHILD: &str = "XYLO_FLOCK_FORK_PROBE";
    if std::env::var_os(CHILD).as_deref() == Some(std::ffi::OsStr::new("1")) {
        inherited_flock_probe();
        return;
    }
    // Exec first so this deliberately held fork cannot inherit unrelated
    // parallel fixtures' authority. The environment is child-local only.
    struct Child(Option<std::process::Child>);
    impl Drop for Child {
        fn drop(&mut self) {
            if let Some(mut child) = self.0.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
    let mut child = Child(Some(Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "action_review::leases::tests::cloexec_fork_reference_keeps_authority_until_child_exit", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn().unwrap()));
    let deadline = Instant::now() + Duration::from_secs(6);
    while child.0.as_mut().unwrap().try_wait().unwrap().is_none() {
        assert!(
            Instant::now() < deadline,
            "Isolated inherited-flock probe did not finish"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let output = child.0.take().unwrap().wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "Isolated inherited-flock probe failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
fn inherited_flock_probe() {
    use std::{
        io::{Read, Write},
        os::{fd::AsRawFd, unix::net::UnixStream},
        time::{Duration, Instant},
    };
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("execution.lock");
    let owner = try_lock(open(&path).unwrap()).unwrap().unwrap();
    let owner_fd = owner.as_raw_fd();
    // SAFETY: the actual owned production lease descriptor is live.
    assert_ne!(
        unsafe { libc::fcntl(owner_fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
        0
    );
    let (mut parent, child) = UnixStream::pair().unwrap();
    parent
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    parent
        .set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let (parent_fd, child_fd) = (parent.as_raw_fd(), child.as_raw_fd());
    // SAFETY: the forked branch below calls only async-signal-safe libc
    // functions and exits without running inherited Rust destructors.
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "Fork probe: {}", std::io::Error::last_os_error());
    if pid == 0 {
        unsafe {
            libc::close(parent_fd);
            if libc::write(child_fd, b"1".as_ptr().cast(), 1) != 1 {
                libc::_exit(2);
            }
            let mut release = [0_u8; 1];
            let read = libc::read(child_fd, release.as_mut_ptr().cast(), 1);
            libc::_exit(if read == 1 || read == 0 { 0 } else { 3 });
        }
    }
    struct ForkOwner(Option<libc::pid_t>);
    impl Drop for ForkOwner {
        fn drop(&mut self) {
            if let Some(pid) = self.0.take() {
                // SAFETY: this is this probe's unreaped direct child.
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                    while libc::waitpid(pid, std::ptr::null_mut(), 0) < 0
                        && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR)
                    {
                    }
                }
            }
        }
    }
    let mut child_owner = ForkOwner(Some(pid));
    drop(child);
    let mut ready = [0_u8; 1];
    parent.read_exact(&mut ready).unwrap();
    assert_eq!(ready, *b"1");
    drop(owner);
    assert!(
        try_lock(open(&path).unwrap()).unwrap().is_none(),
        "CLOEXEC must not erase a still-live fork reference before exec/exit"
    );
    parent.write_all(b"1").unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut status = 0;
    loop {
        // SAFETY: pid is the unreaped direct child, status is valid output.
        let result = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if result == pid {
            child_owner.0 = None;
            break;
        }
        assert!(
            result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR),
            "Wait for fork probe: {}",
            std::io::Error::last_os_error()
        );
        assert!(
            Instant::now() < deadline,
            "Fork owner did not release its descriptor"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(libc::WIFEXITED(status));
    assert_eq!(libc::WEXITSTATUS(status), 0);
    assert!(
        try_lock(open(&path).unwrap()).unwrap().is_some(),
        "Lease becomes available only after every kernel owner exits"
    );
}
