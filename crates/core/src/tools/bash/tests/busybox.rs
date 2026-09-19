//! Busybox backend integration tests. They need a busybox binary behind
//! `AGENTSHIM_TEST_BUSYBOX` (a busybox-w32 build on Windows; a Linux busybox also
//! exercises the dispatcher path) and are skipped when the variable is unset.

use std::{path::PathBuf, time::Instant};

use super::detached::{detach_request, response_job_id};
use super::*;
use crate::tools::bash::status;

fn test_busybox() -> Option<PathBuf> {
    std::env::var_os("AGENTSHIM_TEST_BUSYBOX")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute() && path.is_file())
}

fn busybox_locator(busybox: &std::path::Path) -> BashLocator {
    BashLocator::for_tests(
        Some(busybox.to_owned().into_os_string()),
        Vec::new(),
        std::env::var_os("PATH").unwrap_or_default(),
    )
}

fn busybox_run(busybox: &std::path::Path, command: &str) -> Result<String, ProcessError> {
    let fixture = tempfile::tempdir().expect("fixture");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    execute_output(
        &root,
        &busybox_locator(busybox),
        None,
        &request(command),
        Duration::from_secs(20),
        &CancellationToken::new(),
    )
    .map(|output| output.text)
}

#[test]
fn a_busybox_dispatcher_probes_as_ash_and_executes_through_the_sh_applet() {
    let Some(busybox) = test_busybox() else {
        return;
    };

    let runtime = busybox_locator(&busybox)
        .resolve(&CancellationToken::new())
        .expect("busybox probe");
    assert_eq!(runtime.flavor, locate::ShellFlavor::Ash);
    assert!(runtime.busybox_dispatch);
    assert_eq!(
        runtime.launch_args("true"),
        ["sh".to_owned(), "-c".to_owned(), "true".to_owned()]
    );

    let output = busybox_run(&busybox, "echo hello").expect("busybox run");
    assert!(output.contains("hello"), "{output}");
    assert!(output.contains("Exit code: 0"), "{output}");
}

#[test]
fn a_busybox_applet_alias_probes_as_ash_without_a_dispatch_prefix() {
    let Some(busybox) = test_busybox() else {
        return;
    };
    let fixture = tempfile::tempdir().expect("fixture");
    let alias = fixture
        .path()
        .join(if cfg!(windows) { "sh.exe" } else { "sh" });
    std::fs::copy(&busybox, &alias).expect("alias copy");

    let runtime = busybox_locator(&alias)
        .resolve(&CancellationToken::new())
        .expect("alias probe");
    assert_eq!(runtime.flavor, locate::ShellFlavor::Ash);
    assert!(!runtime.busybox_dispatch);
    assert_eq!(
        runtime.launch_args("true"),
        ["-c".to_owned(), "true".to_owned()]
    );

    let output = busybox_run(&alias, "echo alias").expect("alias run");
    assert!(output.contains("alias"), "{output}");
}

#[test]
fn a_bash_named_busybox_copy_probes_as_ash() {
    let Some(busybox) = test_busybox() else {
        return;
    };
    let fixture = tempfile::tempdir().expect("fixture");
    let shim = fixture
        .path()
        .join(if cfg!(windows) { "bash.exe" } else { "bash" });
    std::fs::copy(&busybox, &shim).expect("shim copy");

    // A build without the `bash` applet alias cannot take this shape; skip it.
    let Ok(runtime) = busybox_locator(&shim)
        .resolve(&CancellationToken::new())
        .inspect_err(|error| eprintln!("bash applet probe failed, skipping: {error:?}"))
    else {
        return;
    };
    assert_eq!(runtime.flavor, locate::ShellFlavor::Ash);
    assert!(!runtime.busybox_dispatch);

    let output = busybox_run(&shim, "echo shim").expect("shim run");
    assert!(output.contains("shim"), "{output}");
}

#[test]
fn busybox_builtin_pipelines_and_command_substitution_run() {
    let Some(busybox) = test_busybox() else {
        return;
    };

    let piped = busybox_run(&busybox, "seq 1 5 | grep 3").expect("pipeline run");
    assert!(piped.contains('3'), "{piped}");

    let substituted =
        busybox_run(&busybox, "v=$(echo hi); echo \"[$v]\"").expect("substitution run");
    assert!(substituted.contains("[hi]"), "{substituted}");
}

#[test]
fn non_ascii_output_survives_the_capture_roundtrip() {
    let Some(busybox) = test_busybox() else {
        return;
    };

    let output = busybox_run(&busybox, "printf 'héllo'").expect("utf-8 run");
    assert!(output.contains("héllo"), "{output}");
}

#[test]
fn a_detached_busybox_tree_runs_to_completion_and_is_terminated() {
    let Some(busybox) = test_busybox() else {
        return;
    };
    let fixture = tempfile::tempdir().expect("fixture");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let trees = trees();

    let admission = trees.admit().expect("admission");
    let response = execute_output(
        &root,
        &busybox_locator(&busybox),
        Some(admission),
        &detach_request("for i in 1 2 3; do echo $i; sleep 1; done", "busybox.log"),
        Duration::from_millis(crate::tools::exec::DEFAULT_TIMEOUT_MS),
        &CancellationToken::new(),
    )
    .expect("detached busybox")
    .text;
    let job_id = response_job_id(&response);

    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let snapshot = trees.status(&job_id, 8192).expect("status");
        if snapshot.state == status::JobState::Completed {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the busybox job did not complete"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let log = std::fs::read_to_string(fixture.path().join("busybox.log")).expect("log");
    for line in ["1", "2", "3"] {
        assert!(log.contains(line), "{line} missing from the log: {log}");
    }

    trees.terminate_all();
    assert_eq!(trees.live_count(), 0);
}

#[test]
fn a_timeout_terminates_busybox_spawned_children() {
    let Some(busybox) = test_busybox() else {
        return;
    };
    let fixture = tempfile::tempdir().expect("fixture");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let mut timed = request("(sleep 2; printf late > late-marker) & printf 'spawned\\n'; wait");
    timed.timeout_ms = Some(750);

    let error = execute_output(
        &root,
        &busybox_locator(&busybox),
        None,
        &timed,
        Duration::from_millis(750),
        &CancellationToken::new(),
    )
    .expect_err("the background wait must run into the timeout");

    assert!(matches!(error, ProcessError::Timeout { .. }), "{error}");
    std::thread::sleep(Duration::from_millis(3_000));
    assert!(
        !fixture.path().join("late-marker").exists(),
        "a busybox-spawned child survived timeout containment"
    );
}
