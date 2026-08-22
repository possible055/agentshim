use super::*;
use crate::tools::bash::{detached as job_registry, status};
use std::time::Instant;

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

pub(super) fn spawn_detached(
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
        Duration::from_millis(crate::tools::exec::DEFAULT_TIMEOUT_MS),
        &CancellationToken::new(),
    )
    .map(|output| output.text)
}

pub(super) fn response_job_id(response: &str) -> String {
    response
        .split_whitespace()
        .find_map(|part| part.strip_prefix("job_id="))
        .expect("detached response job_id")
        .to_owned()
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

    assert!(response.contains("Detached: job_id=bash-"), "{response}");
    assert!(response.contains(" pid="), "{response}");
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

#[test]
fn status_reports_running_log_and_primary_exit_then_retains_completion() {
    if !bash_is_available() {
        return;
    }
    let fixture = tempfile::tempdir().expect("fixture");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let trees = trees();
    let response = spawn_detached(
        &root,
        &trees,
        "printf 'ready\\n'; sleep 1; printf 'done\\n'; exit 7",
        "status.log",
    )
    .expect("detached bash");
    let job_id = response_job_id(&response);

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let terminal = loop {
        let snapshot = trees.status(&job_id, 8192).expect("status");
        if snapshot.state == status::JobState::Completed {
            break snapshot;
        }
        assert!(
            matches!(
                snapshot.state,
                status::JobState::Running
                    | status::JobState::StatusUnknown
                    | status::JobState::Finalizing
            ),
            "unexpected state: {:?}",
            snapshot.state
        );
        assert!(std::time::Instant::now() < deadline, "job did not complete");
        std::thread::sleep(Duration::from_millis(25));
    };

    assert_eq!(terminal.primary_exit.as_deref(), Some("7"));
    assert!(terminal.log.bytes.ends_with(b"done\n"));
    assert_eq!(trees.live_count(), 0);
    assert_eq!(trees.terminal_count(), 1);
    let repeated = trees.status(&job_id, 0).expect("retained status");
    assert_eq!(repeated.state, status::JobState::Completed);
    assert!(repeated.log.bytes.is_empty());
}

#[cfg(windows)]
#[test]
fn windows_completion_refreshes_primary_exit_after_the_job_becomes_empty() {
    if !bash_is_available() {
        return;
    }
    let fixture = tempfile::tempdir().expect("fixture");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let trees = trees();
    let response = spawn_detached(
        &root,
        &trees,
        "while [ ! -f release-exit ]; do sleep 0.01; done; exit 7",
        "exit-race.log",
    )
    .expect("detached bash");
    let job_id = response_job_id(&response);
    let release = fixture.path().join("release-exit");
    crate::platform::process::set_after_primary_observation_hook_for_tests(move || {
        std::fs::write(release, b"go").expect("release primary process");
        std::thread::sleep(Duration::from_millis(250));
    });

    let completed = trees.status(&job_id, 0).expect("completed status");

    assert_eq!(completed.state, status::JobState::Completed);
    assert_eq!(completed.primary_exit.as_deref(), Some("7"));
}

#[test]
fn terminate_is_tree_owned_idempotent_and_unknown_ids_are_rejected() {
    if !bash_is_available() {
        return;
    }
    let fixture = tempfile::tempdir().expect("fixture");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let trees = trees();
    let response = spawn_detached(
        &root,
        &trees,
        "printf 'started\\n'; sleep 30 & sleep 30",
        "terminate.log",
    )
    .expect("detached bash");
    let job_id = response_job_id(&response);

    let snapshot = match trees
        .begin_stop(&job_id, job_registry::StopCause::Explicit)
        .expect("terminate")
    {
        job_registry::StopStart::Accepted(work) => work.run(),
        job_registry::StopStart::Immediate(_) => {
            panic!("running job must yield cleanup owner")
        }
    };
    assert_eq!(snapshot.state, status::JobState::Terminated);
    assert_eq!(trees.live_count(), 0);
    match trees
        .begin_stop(&job_id, job_registry::StopCause::Explicit)
        .expect("repeated terminate")
    {
        job_registry::StopStart::Immediate(snapshot) => {
            assert_eq!(snapshot.state, status::JobState::Terminated);
            assert_eq!(snapshot.outcome, Some("already_terminal"));
        }
        job_registry::StopStart::Accepted(_) => panic!("terminal job cannot be killed twice"),
    }
    let unknown = format!("bash-{}", uuid::Uuid::new_v4());
    assert!(matches!(
        trees.status(&unknown, 0),
        Err(ProcessError::Validation(_))
    ));
}

#[test]
fn timeout_stop_is_distinct_and_dropped_owner_converges_to_terminal() {
    if !bash_is_available() {
        return;
    }
    let fixture = tempfile::tempdir().expect("fixture");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let trees = job_registry::DetachedTrees::new(2);

    let timed =
        spawn_detached(&root, &trees, "sleep 30 & sleep 30", "timed.log").expect("timed job");
    let timed_id = response_job_id(&timed);
    let timed = match trees
        .begin_stop(&timed_id, job_registry::StopCause::Timeout)
        .expect("timeout stop")
    {
        job_registry::StopStart::Accepted(work) => work.run(),
        job_registry::StopStart::Immediate(_) => panic!("running job must yield timeout owner"),
    };
    assert_eq!(timed.state, status::JobState::TimedOut);
    assert_eq!(timed.cause, Some("timeout"));

    let dropped =
        spawn_detached(&root, &trees, "sleep 30", "dropped.log").expect("dropped owner job");
    let dropped_id = response_job_id(&dropped);
    let work = match trees
        .begin_stop(&dropped_id, job_registry::StopCause::Explicit)
        .expect("explicit stop")
    {
        job_registry::StopStart::Accepted(work) => work,
        job_registry::StopStart::Immediate(_) => panic!("running job must yield explicit owner"),
    };
    drop(work);
    let dropped = trees.status(&dropped_id, 0).expect("terminal status");
    assert_eq!(dropped.state, status::JobState::OutcomeUncertain);
    assert_eq!(dropped.cause, Some("explicit"));
    assert_eq!(trees.live_count(), 0);
}

#[test]
fn timeout_terminate_and_shutdown_share_one_first_wins_owner() {
    if !bash_is_available() {
        return;
    }
    let fixture = tempfile::tempdir().expect("fixture");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let trees = job_registry::DetachedTrees::new(1);
    let response =
        spawn_detached(&root, &trees, "sleep 30 & sleep 30", "race.log").expect("race job");
    let job_id = response_job_id(&response);
    let barrier = Arc::new(std::sync::Barrier::new(4));
    let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let mut workers = Vec::new();
    for cause in [
        job_registry::StopCause::Timeout,
        job_registry::StopCause::Explicit,
    ] {
        let worker_trees = trees.clone();
        let worker_id = job_id.clone();
        let worker_barrier = Arc::clone(&barrier);
        let worker_accepted = Arc::clone(&accepted);
        workers.push(std::thread::spawn(move || {
            worker_barrier.wait();
            if let job_registry::StopStart::Accepted(work) = worker_trees
                .begin_stop(&worker_id, cause)
                .expect("race stop")
            {
                worker_accepted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                work.run();
            }
        }));
    }
    let shutdown_trees = trees.clone();
    let shutdown_barrier = Arc::clone(&barrier);
    let shutdown_accepted = Arc::clone(&accepted);
    workers.push(std::thread::spawn(move || {
        shutdown_barrier.wait();
        for work in shutdown_trees.begin_shutdown(Instant::now() + Duration::from_secs(5)) {
            shutdown_accepted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            work.run();
        }
    }));
    barrier.wait();
    for worker in workers {
        worker.join().expect("race worker");
    }

    assert_eq!(accepted.load(std::sync::atomic::Ordering::SeqCst), 1);
    let terminal = trees.status(&job_id, 0).expect("terminal status");
    assert!(matches!(
        terminal.state,
        status::JobState::Terminated | status::JobState::TimedOut
    ));
    assert_eq!(trees.live_count(), 0);
}

#[test]
fn terminal_retention_evicts_the_oldest_job_without_deleting_its_log() {
    if !bash_is_available() {
        return;
    }
    let fixture = tempfile::tempdir().expect("fixture");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let trees = DetachedTrees::new(1);
    let mut ids = Vec::new();
    for index in 0..=job_registry::TERMINAL_RETENTION {
        let log_path = format!("retention-{index}.log");
        let response = spawn_detached(&root, &trees, "true", &log_path).expect("detached bash");
        let job_id = response_job_id(&response);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match trees.status(&job_id, 0).expect("status").state {
                status::JobState::Completed => break,
                _ if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                state => panic!("job did not complete: {state:?}"),
            }
        }
        ids.push(job_id);
    }

    assert_eq!(trees.terminal_count(), job_registry::TERMINAL_RETENTION);
    assert!(matches!(
        trees.status(&ids[0], 0),
        Err(ProcessError::Validation(_))
    ));
    assert!(fixture.path().join("retention-0.log").exists());
    assert!(trees.status(ids.last().expect("latest id"), 0).is_ok());
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
        Duration::from_millis(crate::tools::exec::DEFAULT_TIMEOUT_MS),
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

    let response = spawn_detached(
        &root,
        &trees,
        "while :; do printf x >> degraded-marker; sleep 0.1; done",
        "degraded.log",
    )
    .expect("detached bash");
    let job_id = response_job_id(&response);
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
    let degraded = trees.status(&job_id, 0).expect("degraded status");
    assert_eq!(degraded.state, status::JobState::StatusUnknown);
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

#[test]
fn log_failure_preserves_lifecycle_and_termination_failure_is_retained() {
    if !bash_is_available() {
        return;
    }
    let fixture = tempfile::tempdir().expect("fixture");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let trees = trees();
    let response = spawn_detached(&root, &trees, "sleep 30", "failure.log").expect("detached bash");
    let job_id = response_job_id(&response);

    trees.fail_next_tail_snapshot();
    let status = trees
        .status(&job_id, 8192)
        .expect("status survives log failure");
    assert_eq!(status.state, status::JobState::Running);
    assert!(
        status
            .log
            .error
            .as_deref()
            .is_some_and(|error| error.contains("injected"))
    );
    assert_eq!(trees.live_count(), 1);

    trees.fail_next_termination();
    let terminal = match trees
        .begin_stop(&job_id, job_registry::StopCause::Explicit)
        .expect("terminate")
    {
        job_registry::StopStart::Accepted(work) => work.run(),
        job_registry::StopStart::Immediate(_) => panic!("running job must yield owner"),
    };
    assert_eq!(terminal.state, status::JobState::OutcomeUncertain);
    assert_eq!(trees.live_count(), 0);
    assert_eq!(
        trees
            .status(&job_id, 0)
            .expect("retained uncertain status")
            .state,
        status::JobState::OutcomeUncertain
    );
}

#[test]
fn shutdown_retains_uncertain_pid_from_an_existing_termination_owner() {
    if !bash_is_available() {
        return;
    }
    let fixture = tempfile::tempdir().expect("fixture");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let trees = trees();
    let response =
        spawn_detached(&root, &trees, "sleep 30", "shutdown-owner.log").expect("detached bash");
    let job_id = response_job_id(&response);
    trees.fail_next_termination();
    let work = match trees
        .begin_stop(&job_id, job_registry::StopCause::Explicit)
        .expect("terminate")
    {
        job_registry::StopStart::Accepted(work) => work,
        job_registry::StopStart::Immediate(_) => panic!("running job must yield owner"),
    };
    let pid = work.pid();

    let swept = trees.begin_shutdown(Instant::now() + Duration::from_secs(5));
    assert!(
        swept.is_empty(),
        "the termination owner was already exchanged"
    );
    assert_eq!(trees.shutdown_unverified_pids(), [pid]);
    let terminal = work.run();

    assert_eq!(terminal.state, status::JobState::OutcomeUncertain);
    assert!(trees.wait_until_quiesced(Instant::now() + Duration::from_secs(1)));
    assert_eq!(trees.shutdown_unverified_pids(), [pid]);
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
            Duration::from_millis(crate::tools::exec::DEFAULT_TIMEOUT_MS),
            &CancellationToken::new(),
        )
    });
    entered.wait();
    let swept = trees.begin_shutdown(Instant::now() + crate::tools::exec::spawn::CLEANUP_DEADLINE);
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
    let swept = trees.begin_shutdown(Instant::now() + crate::tools::exec::spawn::CLEANUP_DEADLINE);
    assert!(swept.is_empty());
    assert!(!trees.is_accepting());
    let again = trees.begin_shutdown(Instant::now() + crate::tools::exec::spawn::CLEANUP_DEADLINE);
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
