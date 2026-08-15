use super::*;

pub(super) fn detach_request(command: &str, log_path: &str) -> BashRequest {
    BashRequest {
        command: command.to_owned(),
        cwd: None,
        timeout_ms: None,
        detach: true,
        log_path: Some(log_path.to_owned()),
        msys_argument_conversion: MsysArgumentConversion::Default,
    }
}

fn spawn_detached(
    root: &Arc<RepositoryRoot>,
    trees: &DetachedTrees,
    command: &str,
    log_path: &str,
) -> Result<String, ProcessError> {
    let admission = trees.admit()?;
    let locator = BashLocator::capture();
    execute_output(
        root,
        &locator,
        Some(admission),
        &detach_request(command, log_path),
        Duration::ZERO,
        &CancellationToken::new(),
    )
    .map(|output| output.text)
}

#[test]
fn a_detached_tree_outlives_the_call_and_dies_with_the_instance() {
    if !bash_is_available() {
        return;
    }
    let fixture = tempfile::tempdir().expect("fixture");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let trees = trees();

    let response = spawn_detached(
        &root,
        &trees,
        "printf 'started\n'; sleep 30",
        "detached.log",
    )
    .expect("detached bash");

    assert!(response.contains("Detached: pid="), "{response}");
    assert!(response.contains("detached.log"), "{response}");
    assert!(response.contains("scope="), "{response}");
    assert!(!response.contains("Exit code:"), "{response}");
    assert!(!response.contains("--- output ---"), "{response}");
    assert_eq!(trees.live_count(), 1);

    let log = fixture.path().join("detached.log");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if std::fs::read_to_string(&log).is_ok_and(|body| body.contains("started")) {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        std::fs::read_to_string(&log)
            .expect("log file")
            .contains("started"),
        "the detached tree did not keep running after the call returned"
    );

    trees.terminate_all();
    assert_eq!(trees.live_count(), 0);
}

#[cfg(windows)]
#[test]
fn detached_windows_bash_receives_the_disabled_argument_conversion_environment() {
    if !bash_is_available() {
        return;
    }
    let fixture = tempfile::tempdir().expect("fixture");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let trees = trees();
    let mut request = detach_request("printf '%s\\n' \"$MSYS2_ARG_CONV_EXCL\"", "argument.log");
    request.msys_argument_conversion = MsysArgumentConversion::Disabled;
    let admission = trees.admit().expect("detached admission");
    execute_output(
        &root,
        &BashLocator::capture(),
        Some(admission),
        &request,
        Duration::ZERO,
        &CancellationToken::new(),
    )
    .expect("detached bash");

    let log = fixture.path().join("argument.log");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if std::fs::read_to_string(&log).is_ok_and(|body| body.trim() == "*") {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let output = std::fs::read_to_string(log).expect("argument log");
    assert_eq!(output.trim(), "*");
    trees.terminate_all();
}

#[test]
fn a_full_detached_roster_reports_resource_busy() {
    if !bash_is_available() {
        return;
    }
    let fixture = tempfile::tempdir().expect("fixture");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let trees = DetachedTrees::new(1);

    spawn_detached(&root, &trees, "sleep 30", "first.log").expect("first detached bash");
    let busy = spawn_detached(&root, &trees, "sleep 30", "second.log")
        .expect_err("the single slot is occupied");

    assert!(matches!(busy, ProcessError::ResourceBusy(_)));
    let message = busy.to_string();
    assert!(message.contains("first.log"), "{message}");
    assert!(message.contains("pid "), "{message}");
    trees.terminate_all();
}

/// A finished tree frees its slot at the next admission sweep, so a short-lived command's
/// path and capacity become reusable without any reaper thread.
#[test]
fn a_finished_tree_frees_its_slot_for_the_next_admission() {
    if !bash_is_available() {
        return;
    }
    let fixture = tempfile::tempdir().expect("fixture");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let trees = DetachedTrees::new(1);

    spawn_detached(&root, &trees, "true", "first.log").expect("first detached bash");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let reused = loop {
        match spawn_detached(&root, &trees, "true", "second.log") {
            Ok(reused) => break reused,
            Err(ProcessError::ResourceBusy(_)) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => panic!("unexpected error while waiting for the slot: {error}"),
        }
    };

    assert!(reused.contains("second.log"), "{reused}");
    assert_eq!(trees.live_count(), 1);
    trees.terminate_all();
}

#[test]
fn a_log_path_outside_the_repository_or_without_a_parent_directory_is_rejected() {
    if !bash_is_available() {
        return;
    }
    let fixture = tempfile::tempdir().expect("fixture");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let trees = trees();

    let escaped = spawn_detached(&root, &trees, "true", "../outside.log")
        .expect_err("log_path must stay inside the repository");
    assert!(matches!(escaped, ProcessError::Validation(_)));

    let missing_parent = spawn_detached(&root, &trees, "true", "absent/build.log")
        .expect_err("a missing parent directory is an error, not an implicit mkdir");
    assert!(matches!(missing_parent, ProcessError::Validation(_)));
    assert_eq!(trees.live_count(), 0);
}

/// Two callers must not both see the same free slot. The old admission released the lock
/// before the slot was filled, which let the roster exceed its configured capacity.
#[test]
fn concurrent_detached_admissions_cannot_oversubscribe_the_roster() {
    if !bash_is_available() {
        return;
    }
    let fixture = tempfile::tempdir().expect("fixture");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let trees = DetachedTrees::new(2);
    let barrier = Arc::new(std::sync::Barrier::new(8));

    let workers = (0..8)
        .map(|index| {
            let root = root.clone();
            let trees = trees.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                spawn_detached(&root, &trees, "sleep 30", &format!("{index}.log")).is_ok()
            })
        })
        .collect::<Vec<_>>();
    let admitted = workers
        .into_iter()
        .filter_map(|worker| worker.join().ok())
        .filter(|admitted| *admitted)
        .count();

    assert!(
        admitted <= 2,
        "{admitted} calls were admitted into a roster of 2"
    );
    assert_eq!(trees.live_count(), admitted);
    assert_eq!(trees.reserved_count(), 0, "reservations were leaked");
    trees.terminate_all();
}

/// A failed detached call must return its slot. Without RAII rollback a repository full of
/// bad `log_path` values would permanently consume the roster.
#[test]
fn a_rejected_detached_call_returns_its_slot() {
    if !bash_is_available() {
        return;
    }
    let fixture = tempfile::tempdir().expect("fixture");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let trees = DetachedTrees::new(1);

    for _ in 0..3 {
        spawn_detached(&root, &trees, "true", "absent/build.log")
            .expect_err("a missing parent directory is an error");
        assert_eq!(trees.reserved_count(), 0);
    }
    spawn_detached(&root, &trees, "sleep 30", "after.log")
        .expect("the slot survived three failed admissions");
    trees.terminate_all();
}

#[test]
fn a_detached_spawn_failure_returns_its_reserved_slot() {
    if !bash_is_available() {
        return;
    }
    let fixture = tempfile::tempdir().expect("fixture");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let trees = DetachedTrees::new(1);
    trees.fail_next_spawn();

    let error = spawn_detached(&root, &trees, "sleep 30", "failed.log")
        .expect_err("injected spawn failure");

    assert!(matches!(error, ProcessError::Io(_)));
    assert_eq!(trees.live_count(), 0);
    assert_eq!(trees.reserved_count(), 0);
    spawn_detached(&root, &trees, "sleep 30", "after-failure.log")
        .expect("spawn failure returned the slot");
    trees.terminate_all();
}

/// `RepositoryRoot::resolve` is lexical admission. A link stored inside the repository passes
/// it while pointing outside, so the open itself has to go through the capability.
///
/// The link is deliberately of a kind an unprivileged user can create — a junction on Windows,
/// where a file symlink needs Developer Mode — so this never degrades into a silent skip.
#[test]
fn a_log_path_that_links_out_of_the_repository_cannot_be_written_through() {
    if !bash_is_available() {
        return;
    }
    let fixture = tempfile::tempdir().expect("fixture");
    let outside = tempfile::tempdir().expect("outside fixture");
    let target = outside.path().join("precious.txt");
    std::fs::write(&target, b"must survive").expect("target file");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let requested = link_out_of_root(fixture.path(), outside.path());

    let error = spawn_detached(&root, &trees(), "true", &requested)
        .expect_err("a link out of the repository must not be written through");

    assert!(matches!(error, ProcessError::Validation(_)), "{error}");
    assert_eq!(
        std::fs::read_to_string(&target).expect("target file"),
        "must survive",
        "the detached log truncated a file outside the repository"
    );
}

/// Plant a link inside `root` that leads to `outside`, and return the repository-relative
/// `log_path` that traverses it.
#[cfg(windows)]
fn link_out_of_root(root: &std::path::Path, outside: &std::path::Path) -> String {
    let link = root.join("escape");
    let status = std::process::Command::new("cmd")
        .arg("/c")
        .arg("mklink")
        .arg("/J")
        .arg(&link)
        .arg(outside)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("mklink");
    assert!(
        status.success(),
        "a directory junction needs no elevation and must be creatable here"
    );
    "escape/precious.txt".to_owned()
}

#[cfg(not(windows))]
fn link_out_of_root(root: &std::path::Path, outside: &std::path::Path) -> String {
    std::os::unix::fs::symlink(outside.join("precious.txt"), root.join("escape.log"))
        .expect("symlink");
    "escape.log".to_owned()
}

#[test]
fn cancellation_after_the_initial_check_does_not_truncate_the_log() {
    if !bash_is_available() {
        return;
    }
    let fixture = tempfile::tempdir().expect("fixture");
    let log = fixture.path().join("cancelled.log");
    std::fs::write(&log, "keep me").expect("existing log");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let trees = trees();
    let locator = BashLocator::capture();
    locator
        .resolve(&CancellationToken::new())
        .expect("probed bash");
    let cancellation = CancellationToken::new();
    let entered = Arc::new(std::sync::Barrier::new(2));
    let release = Arc::new(std::sync::Barrier::new(2));
    let hook_entered = Arc::clone(&entered);
    let hook_release = Arc::clone(&release);
    trees.set_before_open_hook(move || {
        hook_entered.wait();
        hook_release.wait();
    });
    let admission = trees.admit().expect("detached admission");
    let worker_root = Arc::clone(&root);
    let worker_locator = locator.clone();
    let worker_cancellation = cancellation.clone();
    let worker = std::thread::spawn(move || {
        execute_output(
            &worker_root,
            &worker_locator,
            Some(admission),
            &detach_request("printf spawned > marker", "cancelled.log"),
            Duration::from_secs(20),
            &worker_cancellation,
        )
    });
    entered.wait();
    cancellation.cancel();
    release.wait();
    let error = worker
        .join()
        .expect("detached worker")
        .expect_err("a cancelled request must not spawn");

    assert!(matches!(error, ProcessError::Cancelled), "{error}");
    assert_eq!(
        std::fs::read_to_string(log).expect("existing log"),
        "keep me"
    );
    assert!(!fixture.path().join("marker").exists());
    assert_eq!(trees.live_count(), 0);
    assert_eq!(trees.reserved_count(), 0, "the slot was not returned");
}

#[test]
fn cancellation_after_capability_open_may_truncate_but_starts_no_tree() {
    if !bash_is_available() {
        return;
    }
    let fixture = tempfile::tempdir().expect("fixture");
    let log = fixture.path().join("cancelled.log");
    std::fs::write(&log, "old contents").expect("existing log");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let trees = trees();
    let locator = BashLocator::capture();
    locator
        .resolve(&CancellationToken::new())
        .expect("probed bash");
    let cancellation = CancellationToken::new();
    let entered = Arc::new(std::sync::Barrier::new(2));
    let release = Arc::new(std::sync::Barrier::new(2));
    let hook_entered = Arc::clone(&entered);
    let hook_release = Arc::clone(&release);
    trees.set_after_open_hook(move || {
        hook_entered.wait();
        hook_release.wait();
    });
    let admission = trees.admit().expect("detached admission");
    let worker_root = Arc::clone(&root);
    let worker_locator = locator.clone();
    let worker_cancellation = cancellation.clone();
    let worker = std::thread::spawn(move || {
        execute_output(
            &worker_root,
            &worker_locator,
            Some(admission),
            &detach_request("printf spawned > marker", "cancelled.log"),
            Duration::from_secs(20),
            &worker_cancellation,
        )
    });
    entered.wait();
    cancellation.cancel();
    release.wait();
    let error = worker
        .join()
        .expect("detached worker")
        .expect_err("a cancelled request must not spawn");

    assert!(matches!(error, ProcessError::Cancelled), "{error}");
    assert_eq!(std::fs::metadata(log).expect("truncated log").len(), 0);
    assert!(!fixture.path().join("marker").exists());
    assert_eq!(trees.live_count(), 0);
    assert_eq!(trees.reserved_count(), 0, "the slot was not returned");
}

/// A foreground call owns its whole tree on both platforms: `&` does not buy a process that
/// outlives the response. `detach` is the supported way to do that.
#[test]
fn a_backgrounded_child_does_not_hold_the_foreground_call_open() {
    if !bash_is_available() {
        return;
    }
    let fixture = tempfile::tempdir().expect("fixture");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let locator = BashLocator::capture();
    let started = std::time::Instant::now();
    let output = execute_output(
        &root,
        &locator,
        None,
        &request("(sleep 1; printf late > delayed-marker) & printf 'parent done\\n'"),
        Duration::from_secs(20),
        &CancellationToken::new(),
    )
    .expect("bash result")
    .text;

    assert!(output.contains("parent done"), "{output}");
    assert!(output.contains("Exit code: 0"), "{output}");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the call waited for a backgrounded child instead of terminating the tree"
    );
    std::thread::sleep(Duration::from_millis(1_250));
    assert!(
        !fixture.path().join("delayed-marker").exists(),
        "the backgrounded child survived long enough to write after the response"
    );
}

/// A failed liveness query is a degraded observation, not an exit report: the roster must
/// keep the job owner and its capacity booked, because dropping the slot would close the
/// only handle and kill a tree that may still be running.
#[test]
fn a_degraded_liveness_query_keeps_the_owner_and_capacity() {
    if !bash_is_available() {
        return;
    }
    let fixture = tempfile::tempdir().expect("fixture");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let trees = trees();

    spawn_detached(
        &root,
        &trees,
        "while :; do printf x >> degraded-marker; sleep 0.1; done",
        "degraded.log",
    )
    .expect("detached bash");
    let marker = fixture.path().join("degraded-marker");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if std::fs::read_to_string(&marker).is_ok_and(|body| !body.is_empty()) {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        !std::fs::read_to_string(&marker)
            .unwrap_or_default()
            .is_empty(),
        "the detached tree did not start"
    );

    trees.fail_next_liveness_query();
    trees
        .admit()
        .expect("the roster has fifteen free slots beside the degraded one");

    assert_eq!(
        trees.live_count(),
        1,
        "a degraded liveness query dropped the owner of a running tree"
    );
    let before = std::fs::read_to_string(&marker).unwrap_or_default().len();
    std::thread::sleep(Duration::from_millis(750));
    let after = std::fs::read_to_string(&marker).unwrap_or_default().len();
    assert!(
        after > before,
        "the running tree was killed by a slot dropped on a query error"
    );
    trees.terminate_all();
}

/// L2/L3: `begin_shutdown` closes admission, and the spawn-to-commit window re-checks it.
/// A tree that committed after the shutdown must be rolled back by the caller — terminated
/// with a bounded, verified wait — never adopted by a roster that has already swept.
#[test]
fn shutdown_racing_a_detached_call_rolls_back_the_late_commit() {
    if !bash_is_available() {
        return;
    }
    let fixture = tempfile::tempdir().expect("fixture");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let trees = trees();
    let locator = BashLocator::capture();
    locator
        .resolve(&CancellationToken::new())
        .expect("probed bash");
    let entered = Arc::new(std::sync::Barrier::new(2));
    let release = Arc::new(std::sync::Barrier::new(2));
    let hook_entered = Arc::clone(&entered);
    let hook_release = Arc::clone(&release);
    trees.set_after_open_hook(move || {
        hook_entered.wait();
        hook_release.wait();
    });
    let admission = trees.admit().expect("detached admission");
    let worker_root = Arc::clone(&root);
    let worker_locator = locator.clone();
    let worker = std::thread::spawn(move || {
        execute_output(
            &worker_root,
            &worker_locator,
            Some(admission),
            &detach_request(
                "while :; do printf x >> late-marker; sleep 0.1; done",
                "late.log",
            ),
            Duration::ZERO,
            &CancellationToken::new(),
        )
    });
    entered.wait();
    let swept = trees.begin_shutdown();
    assert!(swept.is_empty(), "no tree had committed yet");
    assert!(!trees.is_accepting());
    release.wait();
    let error = worker
        .join()
        .expect("detached worker")
        .expect_err("a late commit during shutdown must fail the call");

    assert!(matches!(error, ProcessError::Cancelled), "{error}");
    assert_eq!(trees.live_count(), 0, "the late tree entered the roster");
    assert_eq!(trees.reserved_count(), 0, "the reservation was released");
    std::thread::sleep(Duration::from_millis(1_500));
    assert!(
        std::fs::read_to_string(fixture.path().join("late-marker"))
            .unwrap_or_default()
            .is_empty(),
        "the rolled-back tree kept running after shutdown"
    );
}

#[test]
fn a_roster_that_stopped_accepting_rejects_new_detached_admissions() {
    if !bash_is_available() {
        return;
    }
    let fixture = tempfile::tempdir().expect("fixture");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let trees = trees();

    assert!(trees.is_accepting());
    let swept = trees.begin_shutdown();
    assert!(swept.is_empty());
    assert!(!trees.is_accepting());
    let again = trees.begin_shutdown();
    assert!(again.is_empty(), "the transition is idempotent");

    let error = spawn_detached(&root, &trees, "sleep 30", "after-stop.log")
        .expect_err("a stopped roster must not admit");
    assert!(matches!(error, ProcessError::ResourceBusy(_)), "{error}");
    assert!(error.to_string().contains("stopping"), "{error}");
    assert_eq!(trees.live_count(), 0);
    assert_eq!(trees.reserved_count(), 0);
}

/// Two calls in one instance cannot share an observation pipe: the duplicate is rejected
/// at reservation time, before the log is opened, and the path is reusable once the
/// previous owner has finished.
#[test]
fn an_active_log_path_is_not_reused_and_frees_when_the_owner_finishes() {
    if !bash_is_available() {
        return;
    }
    let fixture = tempfile::tempdir().expect("fixture");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let trees = trees();

    spawn_detached(&root, &trees, "sleep 1", "shared.log").expect("first detached bash");
    let conflict = spawn_detached(&root, &trees, "sleep 30", "shared.log")
        .expect_err("a duplicate log_path must be refused");

    assert!(
        matches!(conflict, ProcessError::ResourceBusy(_)),
        "{conflict}"
    );
    assert!(
        conflict.to_string().contains("already in use"),
        "{conflict}"
    );
    assert_eq!(trees.live_count(), 1);
    assert_eq!(trees.reserved_count(), 0, "the refused call kept its key");

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let reused = loop {
        match spawn_detached(&root, &trees, "true", "shared.log") {
            Ok(reused) => break reused,
            Err(ProcessError::ResourceBusy(_)) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => panic!("unexpected error while waiting for the path: {error}"),
        }
    };

    assert!(reused.contains("shared.log"), "{reused}");
    trees.terminate_all();
}
