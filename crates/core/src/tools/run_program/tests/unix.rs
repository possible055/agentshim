#[cfg(unix)]
use super::*;

#[cfg(unix)]
fn execute_unix(request: &ProcessRequest) -> Result<String, ProcessError> {
    let fixture = tempfile::tempdir().expect("fixture");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    execute(
        &root,
        &ProcessResolver::capture(),
        request,
        Duration::from_millis(
            request.timeout_ms(crate::tools::exec::spawn::default_max_timeout_ms()),
        ),
        &CancellationToken::new(),
    )
}

#[cfg(unix)]
#[test]
fn unix_setup_failures_drop_the_owned_process_group() {
    use process::SetupFailurePoint::{Io, Spawn, Stderr, Stdin, Stdout};

    for point in [Spawn, Stdin, Stdout, Stderr, Io] {
        process::set_setup_failure_for_tests(point);
        let mut failing = request("/bin/sh".to_owned());
        if point == Stdin {
            failing.stdin = Some("input".to_owned());
        }
        failing.args = vec!["-c".to_owned(), "exec sleep 30".to_owned()];
        let error = execute_unix(&failing).expect_err("setup failure must be returned");
        assert!(matches!(error, ProcessError::Io(_)));
        let process_group = process::take_spawned_process_group_for_tests()
            .expect("spawned process group must be recorded");
        assert!(
            !process::process_group_exists_for_tests(process_group),
            "process group {process_group} survived an injected setup failure"
        );
    }
}

#[cfg(unix)]
#[test]
fn unix_native_argv_nonzero_exit_and_environment_are_reported() {
    let mut printf = request("/usr/bin/printf".to_owned());
    printf.args = vec!["[%s]\n".to_owned(), "a b".to_owned(), "&|$".to_owned()];
    let output = execute_unix(&printf).expect("printf");
    assert!(output.contains("[a b]\n[&|$]"));
    assert!(output.contains("Exit code: 0"));

    let mut nonzero = request("/bin/sh".to_owned());
    nonzero.args = vec!["-c".to_owned(), "exit 7".to_owned()];
    let output = execute_unix(&nonzero).expect("nonzero is a completed result");
    assert!(output.contains("Exit code: 7"));

    let mut environment = request("/usr/bin/env".to_owned());
    environment
        .env
        .insert("AGENTSHIM_PROBE".to_owned(), "set".to_owned());
    let output = execute_unix(&environment).expect("environment");
    assert!(output.contains("NO_COLOR=1"));
    assert!(output.contains("AGENTSHIM_PROBE=set"));
}

#[cfg(unix)]
#[test]
fn unix_python_node_and_git_receive_literal_argument_corpus() {
    let corpus = vec!["", "a b", "q\"r", "\\", "界", "&|<>^%!"];
    let expected = serde_json::to_string(&corpus).expect("expected JSON");

    let mut python = request("python3".to_owned());
    python.timeout_ms = Some(10_000);
    python.args = vec![
        "-c".to_owned(),
        "import json,sys; print(json.dumps(sys.argv[1:], ensure_ascii=False, separators=(',', ':')))"
            .to_owned(),
    ];
    python.args.extend(corpus.iter().map(ToString::to_string));
    let output = execute_unix(&python).expect("Python argv probe");
    assert!(output.contains(&expected));

    let mut node = request("node".to_owned());
    node.timeout_ms = Some(10_000);
    node.args = vec![
        "-e".to_owned(),
        "console.log(JSON.stringify(process.argv.slice(1)))".to_owned(),
    ];
    node.args.extend(corpus.iter().map(ToString::to_string));
    let output = execute_unix(&node).expect("Node argv probe");
    assert!(output.contains(&expected));

    let mut git = request("git".to_owned());
    git.timeout_ms = Some(10_000);
    git.args = vec![
        "rev-parse".to_owned(),
        "--sq-quote".to_owned(),
        "a b&|.tmp".to_owned(),
    ];
    let output = execute_unix(&git).expect("Git argv probe");
    assert!(output.contains("a b&|.tmp"));
    assert!(output.contains("Exit code: 0"));
}

#[cfg(unix)]
#[test]
fn explicit_absolute_cwd_may_leave_root_but_relative_escape_is_rejected() {
    let fixture = tempfile::tempdir().expect("root fixture");
    let outside = tempfile::tempdir().expect("outside fixture");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let mut absolute = request("/bin/sh".to_owned());
    absolute.args = vec!["-c".to_owned(), "printf cwd; exit 1".to_owned()];
    absolute.cwd = Some(outside.path().to_string_lossy().into_owned());
    let output = execute(
        &root,
        &ProcessResolver::for_tests(Vec::new()),
        &absolute,
        Duration::from_secs(5),
        &CancellationToken::new(),
    )
    .expect("absolute cwd outside root");
    let expected_cwd = fs::canonicalize(outside.path()).expect("canonical outside cwd");
    assert!(output.contains(&format!("Cwd: {}", expected_cwd.display())));

    absolute.cwd = Some("../outside".to_owned());
    assert!(matches!(
        execute(
            &root,
            &ProcessResolver::for_tests(Vec::new()),
            &absolute,
            Duration::from_secs(5),
            &CancellationToken::new(),
        ),
        Err(ProcessError::Validation(_))
    ));
}

#[cfg(unix)]
#[test]
fn unix_large_stdin_and_both_output_streams_do_not_deadlock() {
    let mut high_output = request("/bin/sh".to_owned());
    high_output.timeout_ms = Some(10_000);
    high_output.stdin = Some("i".repeat(MAX_STDIN_BYTES));
    high_output.args = vec![
        "-c".to_owned(),
        "cat >/dev/null; i=0; while [ $i -lt 4000 ]; do printf 'stdout-%04d-xxxxxxxxxxxxxxxx\n' \"$i\"; printf 'stderr-%04d-yyyyyyyyyyyyyyyy\n' \"$i\" >&2; i=$((i+1)); done".to_owned(),
    ];
    let output = execute_unix(&high_output).expect("high output");
    assert!(output.contains("Exit code: 0"));
    assert!(output.contains("bytes omitted"));
    assert!(output.contains("omitted="));
    assert!(output.len() <= crate::output::MODEL_BYTE_LIMIT);
}

#[cfg(unix)]
#[test]
fn unix_timeout_terminates_descendant_process_group() {
    let fixture = tempfile::tempdir().expect("fixture");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let pid_file = fixture.path().join("descendant.pid");
    let mut timed = request("/bin/sh".to_owned());
    timed.timeout_ms = Some(2_000);
    timed.args = vec![
        "-c".to_owned(),
        format!(
            "printf 'timeout stdout evidence\\n'; printf 'timeout stderr evidence\\n' >&2; sleep 30 & echo $! > '{}'; wait",
            pid_file.display()
        ),
    ];
    let resolver = ProcessResolver::for_tests(Vec::new());
    let resolved_shell = resolver
        .resolve("/bin/sh", root.path())
        .expect("resolve shell fixture");
    let error = execute(
        &root,
        &resolver,
        &timed,
        Duration::from_millis(2_000),
        &CancellationToken::new(),
    )
    .expect_err("timeout");
    assert!(
        matches!(&error, ProcessError::Timeout { .. }),
        "unexpected process error: {error}"
    );
    let report = error.to_string();
    assert!(report.contains(&format!(
        "Resolved program: {}",
        resolved_shell.absolute.display()
    )));
    assert!(report.contains("Cwd:"));
    assert!(report.contains("timeout stdout evidence"));
    assert!(report.contains("timeout stderr evidence"));
    assert!(report.contains("Exit code: unavailable (timed out)"));
    assert!(report.ends_with("Incomplete."));
    assert!(report.len() <= crate::output::MODEL_BYTE_LIMIT);
    let pid = fs::read_to_string(pid_file)
        .expect("descendant pid")
        .trim()
        .parse::<i32>()
        .expect("pid integer");
    let result = unsafe { libc::kill(pid, 0) };
    assert_eq!(result, -1);
    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
}

#[cfg(unix)]
#[test]
fn unix_cancellation_terminates_running_process() {
    let fixture = tempfile::tempdir().expect("fixture");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let mut running = request("/bin/sh".to_owned());
    running.timeout_ms = Some(5_000);
    running.args = vec!["-c".to_owned(), "sleep 30".to_owned()];
    let cancellation = CancellationToken::new();
    let trigger = cancellation.clone();
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        trigger.cancel();
    });
    let error = execute(
        &root,
        &ProcessResolver::for_tests(Vec::new()),
        &running,
        Duration::from_secs(5),
        &cancellation,
    )
    .expect_err("cancelled");
    canceller.join().expect("canceller");
    assert!(matches!(error, ProcessError::Cancelled));
}

#[cfg(unix)]
#[test]
fn unix_session_escape_parent_fixture() {
    if std::env::var("AGENTSHIM_SESSION_ESCAPE_FIXTURE").as_deref() != Ok("parent") {
        return;
    }
    let pid_file = std::env::var_os("AGENTSHIM_SESSION_ESCAPE_PID_FILE").expect("helper PID file");
    let mut command = std::process::Command::new(std::env::current_exe().expect("test executable"));
    command
        .args([
            "--exact",
            "tools::run_program::tests::unix::unix_session_escape_helper_fixture",
            "--nocapture",
        ])
        .env("AGENTSHIM_SESSION_ESCAPE_FIXTURE", "helper")
        .env("AGENTSHIM_SESSION_ESCAPE_PID_FILE", &pid_file);
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn().expect("spawn session-escaping helper");
    drop(child);
    let pid_file = std::path::PathBuf::from(pid_file);
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while !pid_file.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(pid_file.exists(), "session-escaping helper did not start");
}

#[cfg(unix)]
#[test]
fn unix_session_escape_helper_fixture() {
    if std::env::var("AGENTSHIM_SESSION_ESCAPE_FIXTURE").as_deref() != Ok("helper") {
        return;
    }
    let pid_file = std::env::var_os("AGENTSHIM_SESSION_ESCAPE_PID_FILE").expect("helper PID file");
    std::fs::write(pid_file, std::process::id().to_string()).expect("write helper PID");
    std::thread::sleep(Duration::from_secs(30));
}

#[cfg(unix)]
#[test]
fn unix_session_escape_does_not_detach_pipe_workers() {
    let fixture = tempfile::tempdir().expect("fixture");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let pid_file = fixture.path().join("escaped-helper.pid");
    let executable = std::env::current_exe().expect("test executable");
    let mut request = request(executable.to_string_lossy().into_owned());
    request.args = vec![
        "--exact".to_owned(),
        "tools::run_program::tests::unix::unix_session_escape_parent_fixture".to_owned(),
        "--nocapture".to_owned(),
    ];
    request.env.insert(
        "AGENTSHIM_SESSION_ESCAPE_FIXTURE".to_owned(),
        "parent".to_owned(),
    );
    request.env.insert(
        "AGENTSHIM_SESSION_ESCAPE_PID_FILE".to_owned(),
        pid_file.to_string_lossy().into_owned(),
    );
    request.timeout_ms = Some(10_000);
    let started = std::time::Instant::now();

    let error = execute(
        &root,
        &ProcessResolver::capture(),
        &request,
        Duration::from_secs(10),
        &CancellationToken::new(),
    )
    .expect_err("retained pipe must make cleanup uncertain");

    assert!(matches!(error, ProcessError::OutcomeUncertain));
    assert!(started.elapsed() < Duration::from_secs(7));
    let detached_workers = process::active_pipe_workers_for_tests();
    let helper_pid = std::fs::read_to_string(pid_file)
        .expect("helper PID")
        .trim()
        .parse::<i32>()
        .expect("numeric helper PID");
    // SAFETY: The fixture owns this PID and always terminates it before asserting.
    unsafe { libc::kill(helper_pid, libc::SIGKILL) };
    let cleanup_deadline = std::time::Instant::now() + Duration::from_secs(2);
    while process::active_pipe_workers_for_tests() != 0
        && std::time::Instant::now() < cleanup_deadline
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(process::active_pipe_workers_for_tests(), 0);
    assert_eq!(
        detached_workers, 0,
        "completed call detached {detached_workers} pipe workers"
    );
}

#[cfg(unix)]
#[test]
fn unix_stdin_failure_terminates_process_tree_immediately() {
    let mut request = request("/bin/sh".to_owned());
    request.timeout_ms = Some(10_000);
    request.stdin = Some("i".repeat(MAX_STDIN_BYTES));
    request.args = vec!["-c".to_owned(), "exec 0<&-; sleep 30".to_owned()];
    let started = std::time::Instant::now();

    let error = execute_unix(&request).expect_err("stdin failure");

    assert!(matches!(error, ProcessError::Io(_)));
    assert!(started.elapsed() < Duration::from_secs(2));
}
