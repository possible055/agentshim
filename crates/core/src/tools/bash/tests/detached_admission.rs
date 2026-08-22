use super::detached::{detach_request, spawn_detached};
use super::*;

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

#[test]
fn exhausted_response_budget_refuses_before_log_open_or_spawn() {
    if !bash_is_available() {
        return;
    }
    let fixture = tempfile::tempdir().expect("fixture");
    let log = fixture.path().join("budget.log");
    std::fs::write(&log, "keep me").expect("existing log");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let trees = trees();
    let budget = crate::output::TestCallBudget {
        ceiling: 0,
        ..crate::output::TestCallBudget::default()
    };
    let admission = trees.admit().expect("detached admission");

    let error = execute_output_with_budget(
        &root,
        &BashLocator::capture(),
        Some(admission),
        &detach_request("printf spawned > marker", "budget.log"),
        Duration::from_millis(crate::tools::exec::DEFAULT_TIMEOUT_MS),
        &CancellationToken::new(),
        &budget,
    )
    .expect_err("an unreportable job must not start");

    assert!(
        matches!(
            error,
            ProcessError::Output(crate::output::OutputError::BurstLimit)
        ),
        "{error}"
    );
    assert_eq!(
        std::fs::read_to_string(log).expect("preserved log"),
        "keep me"
    );
    assert!(!fixture.path().join("marker").exists());
    assert_eq!(trees.live_count(), 0);
    assert_eq!(trees.reserved_count(), 0);
}
