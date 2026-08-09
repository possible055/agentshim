use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};
#[cfg(windows)]
use std::{env, process::Command, thread};
#[cfg(unix)]
use std::{fs, os::unix::process::CommandExt};
#[cfg(unix)]
use std::{io, path::Path};

use tokio_util::sync::CancellationToken;

use crate::{
    path::RepositoryRoot,
    tools::{
        ToolOutput,
        exec::{
            ProcessError, ProcessResolver,
            capture::{Capture, capture_bytes_per_stream},
            resolve::{Launcher, ResolvedProgram},
        },
    },
};

#[cfg(unix)]
use crate::tools::exec::platform;

use super::{
    AllowedPrograms, CompletedProcess, MAX_STDIN_BYTES, PROCESS_MEMORY_BYTES, ProcessRequest,
    TimedOutProcess, execute, execute_output, render_completed, render_timeout,
};
fn request(program: String) -> ProcessRequest {
    ProcessRequest {
        program,
        args: Vec::new(),
        cwd: None,
        env: BTreeMap::new(),
        unset_env: Vec::new(),
        stdin: None,
        timeout_ms: Some(2_000),
    }
}

fn completed_output(stdout: &[u8], stderr: &[u8], exit: &str) -> ToolOutput {
    completed_output_with_paths(stdout, stderr, exit, "tool", "workspace")
}

fn completed_output_with_paths(
    stdout: &[u8],
    stderr: &[u8],
    exit: &str,
    program: &str,
    cwd: &str,
) -> ToolOutput {
    let mut stdout_capture = Capture::new(capture_bytes_per_stream(2));
    stdout_capture.push(stdout);
    let mut stderr_capture = Capture::new(capture_bytes_per_stream(2));
    stderr_capture.push(stderr);
    render_completed(
        &CompletedProcess {
            resolved: ResolvedProgram {
                absolute: PathBuf::from(program),
                executable: PathBuf::from(program),
                launcher: Launcher::Native,
            },
            cwd: PathBuf::from(cwd),
            exit: exit.to_owned(),
            duration: Duration::from_millis(1),
            stdout: stdout_capture,
            stderr: stderr_capture,
        },
        &CancellationToken::new(),
    )
    .expect("render completed process")
}

fn shown_bytes(output: &str, stream: &str) -> usize {
    let prefix = format!("{stream} bytes: ");
    let line = output
        .lines()
        .find(|line| line.starts_with(&prefix))
        .unwrap_or_else(|| panic!("missing {stream} statistics"));
    line.split("shown=")
        .nth(1)
        .and_then(|value| value.split(',').next())
        .and_then(|value| value.parse().ok())
        .expect("shown byte count")
}

#[test]
fn validation_rejects_conflicts_nul_and_oversized_stdin() {
    let mut invalid = request("tool".to_owned());
    invalid.env.insert("Path".to_owned(), "value".to_owned());
    invalid.unset_env.push("Path".to_owned());
    assert!(matches!(
        invalid.validate(),
        Err(ProcessError::Validation(_))
    ));

    invalid.env.clear();
    invalid.unset_env.clear();
    invalid.args.push("nul\0arg".to_owned());
    assert!(matches!(
        invalid.validate(),
        Err(ProcessError::Validation(_))
    ));

    invalid.args.clear();
    invalid.stdin = Some("x".repeat(MAX_STDIN_BYTES + 1));
    assert!(matches!(
        invalid.validate(),
        Err(ProcessError::Validation(_))
    ));
}

#[test]
fn process_memory_charge_includes_the_capture_and_render_reservation() {
    let request = request("tool".to_owned());
    assert_eq!(request.memory_charge(), PROCESS_MEMORY_BYTES + "tool".len());
}

#[cfg(unix)]
#[test]
fn unix_multicall_proxy_preserves_resolved_argv0() {
    use std::os::unix::fs::symlink;

    let fixture = tempfile::tempdir().expect("fixture");
    let proxy = fixture.path().join("cargo");
    symlink(std::env::current_exe().expect("test executable"), &proxy)
        .expect("create multicall proxy");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let resolver = ProcessResolver::for_tests(vec![fixture.path().to_owned()]);
    let mut proxy_request = request("cargo".to_owned());
    proxy_request.args = vec![
        "--exact".to_owned(),
        "tools::run_program::tests::unix_multicall_argv0_child_fixture".to_owned(),
        "--nocapture".to_owned(),
    ];
    proxy_request
        .env
        .insert("CODEXSHIM_MULTICALL_FIXTURE".to_owned(), "child".to_owned());

    let output = execute(
        &root,
        &resolver,
        &allow_program(&proxy_request),
        &proxy_request,
        Duration::from_secs(5),
        &CancellationToken::new(),
    )
    .expect("multicall proxy");

    let expected_proxy = fs::canonicalize(proxy.parent().expect("proxy parent"))
        .expect("canonical proxy parent")
        .join(proxy.file_name().expect("proxy name"));
    assert!(output.contains(&format!("Resolved program: {}", expected_proxy.display())));
    assert!(output.contains("multicall argv0: cargo"));
    assert!(output.contains("Exit code: 0"));
}

#[cfg(unix)]
#[test]
fn unix_multicall_argv0_child_fixture() {
    if std::env::var("CODEXSHIM_MULTICALL_FIXTURE").as_deref() != Ok("child") {
        return;
    }
    let argv0 = std::env::args_os().next().expect("argv0");
    let name = Path::new(&argv0)
        .file_name()
        .and_then(|value| value.to_str())
        .expect("UTF-8 argv0 name");
    assert_eq!(name, "cargo");
    println!("multicall argv0: {name}");
}

#[test]
fn timeout_report_is_bounded_and_preserves_required_diagnostics() {
    let mut stdout = Capture::new(capture_bytes_per_stream(2));
    stdout.push(&vec![b'o'; crate::output::MODEL_BYTE_LIMIT * 2]);
    let mut stderr = Capture::new(capture_bytes_per_stream(2));
    stderr.push(b"timeout stderr evidence\n");
    let report = render_timeout(
        &TimedOutProcess {
            resolved: ResolvedProgram {
                absolute: PathBuf::from("cargo"),
                executable: PathBuf::from("cargo"),
                launcher: Launcher::Native,
            },
            cwd: PathBuf::from("workspace"),
            duration: Duration::from_millis(150),
            stdout,
            stderr,
        },
        150,
    )
    .expect("timeout report");

    assert!(report.contains("Resolved program: cargo"));
    assert!(report.contains("Launcher: native"));
    assert!(report.contains("Cwd: workspace"));
    assert!(report.contains("Exit code: unavailable (timed out)"));
    assert!(report.contains("timeout stderr evidence"));
    assert!(report.ends_with("Incomplete."));
    assert!(report.len() <= crate::output::MODEL_BYTE_LIMIT);
}

#[test]
fn before_spawn_timeout_does_not_claim_process_diagnostics() {
    let message = ProcessError::TimeoutBeforeSpawn { timeout_ms: 25 }.to_string();
    assert!(message.contains("no child was started"));
    for field in ["Resolved program:", "Launcher:", "Cwd:", "Exit code:"] {
        assert!(!message.contains(field));
    }
}

/// `render_completed` appends the header and the byte-statistics tail after projection, and
/// `diagnostic_path` allows each path up to 2 KiB. A production `output_invariant` failure on
/// 2026-08-08 traced to this path, so the near-limit header case stays covered.
#[test]
fn a_header_near_the_diagnostic_path_limit_still_fits_the_result_budget() {
    for (program, cwd) in [
        ("p".repeat(4_096), "c".repeat(4_096)),
        ("程式".repeat(2_048), "路徑".repeat(2_048)),
    ] {
        let output = completed_output_with_paths(
            &vec![b'x'; 200_000],
            &vec![b'y'; 200_000],
            "0",
            &program,
            &cwd,
        );

        assert!(
            output.fits_budget(),
            "header near the limit broke the budget"
        );
        assert!(output.text.contains("...[path truncated]..."));
        assert!(output.text.contains("Resolved program: "));
        assert!(output.text.contains("Exit code: 0"));
        assert!(output.text.ends_with("Complete."));
    }
}

#[test]
fn cjk_heavy_process_output_is_bounded_below_the_english_budget() {
    let english = completed_output(&"build succeeded\n".repeat(8_000).into_bytes(), b"", "0");
    let chinese = completed_output(&"建置成功且輸出很長\n".repeat(8_000).into_bytes(), b"", "0");

    assert!(english.fits_budget());
    assert!(chinese.fits_budget());
    assert!(
        chinese.encoded_len() < english.encoded_len(),
        "CJK output must be held to a smaller byte budget than English output"
    );
}

#[test]
fn single_stream_output_that_fits_is_returned_in_full() {
    let stdout = vec![b'x'; 8_908];

    let output = completed_output(&stdout, b"", "0");

    assert!(output.fits_budget());
    let stdout_text = output
        .text
        .split_once("--- stdout ---\n")
        .and_then(|(_, output)| output.split_once("\n--- stderr ---"))
        .map(|(stdout, _)| stdout)
        .expect("stdout section");
    assert_eq!(stdout_text.as_bytes(), stdout);
    assert!(
        output
            .text
            .contains("Stdout bytes: total=8908, shown=8908, omitted=0, invalid=0")
    );
    assert!(!output.text.contains("bytes omitted"));
}

#[test]
fn dual_high_output_is_fair_and_fits_the_complete_result_budget() {
    let stdout = vec![b'x'; 100_000];
    let stderr = vec![b'y'; 100_000];

    let output = completed_output(&stdout, &stderr, "0");

    assert!(output.fits_budget());
    assert!(output.text.matches("bytes omitted").count() >= 2);
    let stdout_shown = shown_bytes(&output.text, "Stdout");
    let stderr_shown = shown_bytes(&output.text, "Stderr");
    assert_eq!(stdout_shown, stderr_shown);
    assert!(stdout_shown > 6 * 1024);
}

#[test]
fn escaped_and_invalid_bytes_cannot_break_the_output_budget() {
    let cases = [
        vec![b'\\'; 100_000],
        vec![b'"'; 100_000],
        vec![0x01; 100_000],
        vec![0xFF; 100_000],
        (0..100_000)
            .map(|index| [b'\\', b'"', 0x01, 0xFF, b'\n'][index % 5])
            .collect(),
    ];

    for bytes in cases {
        let output = completed_output(&bytes, &bytes, "0");
        assert!(output.fits_budget());
        assert!(output.text.ends_with("Complete."));
        assert!(shown_bytes(&output.text, "Stdout") > 0);
        assert!(shown_bytes(&output.text, "Stderr") > 0);
    }
}

#[test]
fn small_stderr_is_preserved_before_stdout_uses_remaining_budget() {
    let stdout = vec![b'x'; 100_000];
    let stderr = b"critical diagnostic\n";

    let output = completed_output(&stdout, stderr, "7");

    assert!(output.fits_budget());
    assert!(output.child_nonzero);
    assert!(output.text.contains("critical diagnostic"));
    assert!(
        output
            .text
            .contains("Stderr bytes: total=20, shown=20, omitted=0, invalid=0")
    );
    assert!(output.text.contains("Exit code: 7"));
}

#[test]
fn timeout_projection_fits_the_complete_error_envelope() {
    let mut stdout = Capture::new(capture_bytes_per_stream(2));
    stdout.push(&vec![b'\\'; 100_000]);
    let mut stderr = Capture::new(capture_bytes_per_stream(2));
    stderr.push(&vec![0xFF; 100_000]);
    let report = render_timeout(
        &TimedOutProcess {
            resolved: ResolvedProgram {
                absolute: PathBuf::from("tool"),
                executable: PathBuf::from("tool"),
                launcher: Launcher::Native,
            },
            cwd: PathBuf::from("workspace"),
            duration: Duration::from_millis(150),
            stdout,
            stderr,
        },
        150,
    )
    .expect("timeout report");
    let details = serde_json::to_value(&report.details).expect("timeout details");

    assert!(crate::output::tool_error_result_fits_budget(
        "resource_timeout",
        true,
        &report.text,
        Some(&details)
    ));
    assert_eq!(
        report.details.stdout.shown + report.details.stdout.omitted,
        report.details.stdout.total
    );
    assert_eq!(
        report.details.stderr.shown + report.details.stderr.omitted,
        report.details.stderr.total
    );
    assert!(details["stdout"].get("text").is_none());
    assert!(report.text.ends_with("Incomplete."));
}

#[cfg(any(unix, windows))]
#[test]
fn high_escaping_output_child_fixture() {
    use std::io::Write as _;

    if std::env::var("CODEXSHIM_OUTPUT_FIXTURE").as_deref() != Ok("child") {
        return;
    }
    let bytes = [b'\\', b'"', 0x01, 0xFF, b'\n']
        .into_iter()
        .cycle()
        .take(100_000)
        .collect::<Vec<_>>();
    std::io::stdout()
        .write_all(&bytes)
        .expect("write stdout fixture");
    std::io::stderr()
        .write_all(&bytes)
        .expect("write stderr fixture");
}

#[cfg(any(unix, windows))]
#[test]
fn interleaved_output_child_fixture() {
    use std::io::Write as _;

    if std::env::var("CODEXSHIM_INTERLEAVE_FIXTURE").as_deref() != Ok("child") {
        return;
    }
    for index in 1..=6 {
        if index % 2 == 0 {
            writeln!(std::io::stderr(), "e{index}").expect("write stderr fixture");
            std::io::stderr().flush().expect("flush stderr fixture");
        } else {
            writeln!(std::io::stdout(), "o{index}").expect("write stdout fixture");
            std::io::stdout().flush().expect("flush stdout fixture");
        }
    }
}

/// The counterpart to the bash merged-pipe test: the same interleaved writer must still be
/// reported as two separate sections here.
#[cfg(any(unix, windows))]
#[test]
fn interleaved_child_output_stays_split_into_two_sections() {
    let fixture = tempfile::tempdir().expect("fixture");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let executable = std::env::current_exe().expect("test executable");
    let mut interleaved = request(executable.to_string_lossy().into_owned());
    interleaved.timeout_ms = Some(10_000);
    interleaved.args = vec![
        "--exact".to_owned(),
        "tools::run_program::tests::interleaved_output_child_fixture".to_owned(),
        "--nocapture".to_owned(),
    ];
    interleaved.env.insert(
        "CODEXSHIM_INTERLEAVE_FIXTURE".to_owned(),
        "child".to_owned(),
    );

    let output = execute(
        &root,
        &ProcessResolver::capture(),
        &allow_program(&interleaved),
        &interleaved,
        Duration::from_secs(10),
        &CancellationToken::new(),
    )
    .expect("interleaved output");

    let stdout_section = output
        .split_once("--- stdout ---\n")
        .and_then(|(_, rest)| rest.split_once("\n--- stderr ---"))
        .map(|(section, _)| section.to_owned())
        .expect("stdout section");
    let stderr_section = output
        .split_once("--- stderr ---\n")
        .and_then(|(_, rest)| rest.split_once("\nExit code:"))
        .map(|(section, _)| section.to_owned())
        .expect("stderr section");

    for expected in ["o1", "o3", "o5"] {
        assert!(stdout_section.contains(expected), "{output}");
        assert!(!stderr_section.contains(expected), "{output}");
    }
    for expected in ["e2", "e4", "e6"] {
        assert!(stderr_section.contains(expected), "{output}");
        assert!(!stdout_section.contains(expected), "{output}");
    }
}

#[cfg(any(unix, windows))]
#[test]
fn high_escaping_child_output_completes_within_the_result_budget() {
    let fixture = tempfile::tempdir().expect("fixture");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let executable = std::env::current_exe().expect("test executable");
    let mut high_output = request(executable.to_string_lossy().into_owned());
    high_output.timeout_ms = Some(10_000);
    high_output.args = vec![
        "--exact".to_owned(),
        "tools::run_program::tests::high_escaping_output_child_fixture".to_owned(),
        "--nocapture".to_owned(),
    ];
    high_output
        .env
        .insert("CODEXSHIM_OUTPUT_FIXTURE".to_owned(), "child".to_owned());

    let output = execute_output(
        &root,
        &ProcessResolver::capture(),
        &allow_program(&high_output),
        &high_output,
        Duration::from_secs(10),
        &CancellationToken::new(),
    )
    .expect("high escaping output");

    assert!(output.fits_budget());
    assert!(output.text.contains("bytes omitted"));
    assert!(output.text.ends_with("Complete."));
}

#[cfg(unix)]
fn execute_unix(request: &ProcessRequest) -> Result<String, ProcessError> {
    let fixture = tempfile::tempdir().expect("fixture");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    execute(
        &root,
        &ProcessResolver::capture(),
        &allow_program(request),
        request,
        Duration::from_millis(request.timeout_ms()),
        &CancellationToken::new(),
    )
}

#[cfg(unix)]
#[test]
fn unix_setup_failures_drop_the_owned_process_group() {
    use platform::SetupFailurePoint::{Io, Spawn, Stderr, Stdin, Stdout};

    for point in [Spawn, Stdin, Stdout, Stderr, Io] {
        platform::set_setup_failure_for_tests(point);
        let mut failing = request("/bin/sh".to_owned());
        failing.args = vec!["-c".to_owned(), "exec sleep 30".to_owned()];
        let error = execute_unix(&failing).expect_err("setup failure must be returned");
        assert!(matches!(error, ProcessError::Io(_)));
        let process_group = platform::take_spawned_process_group_for_tests()
            .expect("spawned process group must be recorded");
        assert!(
            !platform::process_group_exists_for_tests(process_group),
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
    assert!(output.contains("Launcher: native"));
    assert!(output.contains("Exit code: 0"));

    let mut nonzero = request("/bin/sh".to_owned());
    nonzero.args = vec!["-c".to_owned(), "exit 7".to_owned()];
    let output = execute_unix(&nonzero).expect("nonzero is a completed result");
    assert!(output.contains("Exit code: 7"));

    let mut environment = request("/usr/bin/env".to_owned());
    environment
        .env
        .insert("CODEXSHIM_PROBE".to_owned(), "set".to_owned());
    let output = execute_unix(&environment).expect("environment");
    assert!(output.contains("NO_COLOR=1"));
    assert!(output.contains("CODEXSHIM_PROBE=set"));
}

#[cfg(unix)]
#[test]
fn unix_python_node_and_git_receive_literal_argument_corpus() {
    let corpus = vec!["", "a b", "q\"r", "\\", "界", "&|<>^%!"];
    let expected = serde_json::to_string(&corpus).expect("expected JSON");

    let mut python = request("python3".to_owned());
    python.args = vec![
        "-c".to_owned(),
        "import json,sys; print(json.dumps(sys.argv[1:], ensure_ascii=False, separators=(',', ':')))"
            .to_owned(),
    ];
    python.args.extend(corpus.iter().map(ToString::to_string));
    let output = execute_unix(&python).expect("Python argv probe");
    assert!(output.contains(&expected));

    let mut node = request("node".to_owned());
    node.args = vec![
        "-e".to_owned(),
        "console.log(JSON.stringify(process.argv.slice(1)))".to_owned(),
    ];
    node.args.extend(corpus.iter().map(ToString::to_string));
    let output = execute_unix(&node).expect("Node argv probe");
    assert!(output.contains(&expected));

    let mut git = request("git".to_owned());
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
    let mut absolute = request("/usr/bin/printf".to_owned());
    absolute.args = vec!["cwd".to_owned()];
    absolute.cwd = Some(outside.path().to_string_lossy().into_owned());
    let output = execute(
        &root,
        &ProcessResolver::for_tests(Vec::new()),
        &allow_program(&absolute),
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
            &allow_program(&absolute),
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
    timed.timeout_ms = Some(150);
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
        &allow_program(&timed),
        &timed,
        Duration::from_millis(150),
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
        &allow_program(&running),
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
    if std::env::var("CODEXSHIM_SESSION_ESCAPE_FIXTURE").as_deref() != Ok("parent") {
        return;
    }
    let pid_file = std::env::var_os("CODEXSHIM_SESSION_ESCAPE_PID_FILE").expect("helper PID file");
    let mut command = std::process::Command::new(std::env::current_exe().expect("test executable"));
    command
        .args([
            "--exact",
            "tools::run_program::tests::unix_session_escape_helper_fixture",
            "--nocapture",
        ])
        .env("CODEXSHIM_SESSION_ESCAPE_FIXTURE", "helper")
        .env("CODEXSHIM_SESSION_ESCAPE_PID_FILE", &pid_file);
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
    if std::env::var("CODEXSHIM_SESSION_ESCAPE_FIXTURE").as_deref() != Ok("helper") {
        return;
    }
    let pid_file = std::env::var_os("CODEXSHIM_SESSION_ESCAPE_PID_FILE").expect("helper PID file");
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
        "tools::run_program::tests::unix_session_escape_parent_fixture".to_owned(),
        "--nocapture".to_owned(),
    ];
    request.env.insert(
        "CODEXSHIM_SESSION_ESCAPE_FIXTURE".to_owned(),
        "parent".to_owned(),
    );
    request.env.insert(
        "CODEXSHIM_SESSION_ESCAPE_PID_FILE".to_owned(),
        pid_file.to_string_lossy().into_owned(),
    );
    request.timeout_ms = Some(10_000);
    let started = std::time::Instant::now();

    let error = execute(
        &root,
        &ProcessResolver::capture(),
        &allow_program(&request),
        &request,
        Duration::from_secs(10),
        &CancellationToken::new(),
    )
    .expect_err("retained pipe must make cleanup uncertain");

    assert!(matches!(error, ProcessError::OutcomeUncertain));
    assert!(started.elapsed() < Duration::from_secs(7));
    let detached_workers = platform::active_pipe_workers_for_tests();
    let helper_pid = std::fs::read_to_string(pid_file)
        .expect("helper PID")
        .trim()
        .parse::<i32>()
        .expect("numeric helper PID");
    // SAFETY: The fixture owns this PID and always terminates it before asserting.
    unsafe { libc::kill(helper_pid, libc::SIGKILL) };
    let cleanup_deadline = std::time::Instant::now() + Duration::from_secs(2);
    while platform::active_pipe_workers_for_tests() != 0
        && std::time::Instant::now() < cleanup_deadline
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(platform::active_pipe_workers_for_tests(), 0);
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

#[cfg(windows)]
#[test]
fn windows_grandchild_child_fixture() {
    if env::var("CODEXSHIM_PROCESS_FIXTURE").as_deref() != Ok("child") {
        return;
    }
    let pid_file = env::var_os("CODEXSHIM_PROCESS_PID_FILE").expect("pid file");
    std::fs::write(pid_file, std::process::id().to_string()).expect("write child pid");
    thread::sleep(Duration::from_secs(30));
}

#[cfg(windows)]
#[test]
fn windows_grandchild_parent_fixture() {
    use std::io::Write as _;

    if env::var("CODEXSHIM_PROCESS_FIXTURE").as_deref() != Ok("parent") {
        return;
    }
    writeln!(std::io::stdout(), "timeout stdout evidence").expect("write stdout evidence");
    std::io::stdout().flush().expect("flush stdout evidence");
    writeln!(std::io::stderr(), "timeout stderr evidence").expect("write stderr evidence");
    std::io::stderr().flush().expect("flush stderr evidence");
    let executable = env::current_exe().expect("test executable");
    let status = Command::new(executable)
        .args([
            "--exact",
            "tools::run_program::tests::windows_grandchild_child_fixture",
            "--nocapture",
        ])
        .env("CODEXSHIM_PROCESS_FIXTURE", "child")
        .env(
            "CODEXSHIM_PROCESS_PID_FILE",
            env::var_os("CODEXSHIM_PROCESS_PID_FILE").expect("pid file"),
        )
        .status()
        .expect("spawn child fixture");
    assert!(status.success());
}

#[cfg(windows)]
#[test]
fn windows_lingering_grandchild_parent_fixture() {
    if env::var("CODEXSHIM_PROCESS_FIXTURE").as_deref() != Ok("lingering-parent") {
        return;
    }
    let pid_file = env::var_os("CODEXSHIM_PROCESS_PID_FILE").expect("pid file");
    let executable = env::current_exe().expect("test executable");
    let child = Command::new(executable)
        .args([
            "--exact",
            "tools::run_program::tests::windows_grandchild_child_fixture",
            "--nocapture",
        ])
        .env("CODEXSHIM_PROCESS_FIXTURE", "child")
        .env("CODEXSHIM_PROCESS_PID_FILE", &pid_file)
        .spawn()
        .expect("spawn lingering child fixture");
    let pid_file = std::path::PathBuf::from(pid_file);
    let started = std::time::Instant::now();
    while !pid_file.exists() && started.elapsed() < Duration::from_secs(2) {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(pid_file.exists(), "lingering child did not start");
    drop(child);
}

#[cfg(windows)]
fn windows_process_is_running(pid: u32) -> bool {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    };

    const STILL_ACTIVE_EXIT_CODE: u32 = 259;
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return false;
    }
    let mut exit_code = 0_u32;
    let succeeded = unsafe { GetExitCodeProcess(handle, &raw mut exit_code) } != 0;
    unsafe {
        CloseHandle(handle);
    }
    succeeded && exit_code == STILL_ACTIVE_EXIT_CODE
}

#[cfg(windows)]
#[test]
fn windows_primary_exit_terminates_lingering_grandchild() {
    let fixture = tempfile::tempdir().expect("fixture");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let pid_file = fixture.path().join("lingering-grandchild.pid");
    let executable = env::current_exe().expect("test executable");
    let mut request = request(executable.to_string_lossy().into_owned());
    request.args = vec![
        "--exact".to_owned(),
        "tools::run_program::tests::windows_lingering_grandchild_parent_fixture".to_owned(),
        "--nocapture".to_owned(),
    ];
    request.env.insert(
        "CODEXSHIM_PROCESS_FIXTURE".to_owned(),
        "lingering-parent".to_owned(),
    );
    request.env.insert(
        "CODEXSHIM_PROCESS_PID_FILE".to_owned(),
        pid_file.to_string_lossy().into_owned(),
    );
    request.timeout_ms = Some(5_000);
    let started = std::time::Instant::now();

    let output = execute(
        &root,
        &ProcessResolver::capture(),
        &allow_program(&request),
        &request,
        Duration::from_secs(5),
        &CancellationToken::new(),
    )
    .expect("completed primary process");

    assert!(output.contains("Exit code: 0"));
    assert!(started.elapsed() < Duration::from_secs(2));
    let pid = std::fs::read_to_string(pid_file)
        .expect("lingering child pid")
        .trim()
        .parse::<u32>()
        .expect("pid integer");
    assert!(
        !windows_process_is_running(pid),
        "lingering grandchild survived primary completion"
    );
}

#[cfg(windows)]
#[test]
fn windows_timeout_terminates_grandchild_job_tree() {
    let fixture = tempfile::tempdir().expect("fixture");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let pid_file = fixture.path().join("grandchild.pid");
    let executable = env::current_exe().expect("test executable");
    let mut timed = request(executable.to_string_lossy().into_owned());
    timed.args = vec![
        "--exact".to_owned(),
        "tools::run_program::tests::windows_grandchild_parent_fixture".to_owned(),
        "--nocapture".to_owned(),
    ];
    timed
        .env
        .insert("CODEXSHIM_PROCESS_FIXTURE".to_owned(), "parent".to_owned());
    timed.env.insert(
        "CODEXSHIM_PROCESS_PID_FILE".to_owned(),
        pid_file.to_string_lossy().into_owned(),
    );
    timed.timeout_ms = Some(750);
    let error = execute(
        &root,
        &ProcessResolver::capture(),
        &allow_program(&timed),
        &timed,
        Duration::from_millis(750),
        &CancellationToken::new(),
    )
    .expect_err("timeout");
    assert!(
        matches!(&error, ProcessError::Timeout { .. }),
        "unexpected process error: {error}"
    );
    let report = error.to_string();
    assert!(report.contains("Resolved program:"));
    assert!(report.contains("Launcher: native"));
    assert!(report.contains("Cwd:"));
    assert!(report.contains("timeout stdout evidence"));
    assert!(report.contains("timeout stderr evidence"));
    assert!(report.contains("Exit code: unavailable (timed out)"));
    assert!(report.ends_with("Incomplete."));
    assert!(report.len() <= crate::output::MODEL_BYTE_LIMIT);
    let pid = std::fs::read_to_string(pid_file)
        .expect("grandchild pid")
        .trim()
        .parse::<u32>()
        .expect("pid integer");
    assert!(
        !windows_process_is_running(pid),
        "grandchild process survived job termination"
    );
}

fn allowlist(entries: &[&str]) -> AllowedPrograms {
    AllowedPrograms::parse(&entries.join(",")).expect("allowlist")
}

fn allow_program(request: &ProcessRequest) -> AllowedPrograms {
    allowlist(&[request.program.as_str()])
}

fn resolved(invocation: impl Into<PathBuf>, executable: impl Into<PathBuf>) -> ResolvedProgram {
    ResolvedProgram {
        absolute: invocation.into(),
        executable: executable.into(),
        launcher: Launcher::Native,
    }
}

#[test]
fn an_empty_allowlist_denies_every_program() {
    let fixture = tempfile::tempdir().expect("fixture");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let executable = std::env::current_exe().expect("test executable");
    let mut denied = request(executable.to_string_lossy().into_owned());
    denied.args = vec!["--version".to_owned()];

    let error = execute(
        &root,
        &ProcessResolver::capture(),
        &AllowedPrograms::default(),
        &denied,
        Duration::from_secs(5),
        &CancellationToken::new(),
    )
    .expect_err("the default allowlist denies everything");

    assert!(matches!(error, ProcessError::NotPermitted(_)));
    let message = error.to_string();
    assert!(message.contains("Use bash"), "{message}");
    assert!(message.contains("--allow-programs"), "{message}");
}

#[test]
fn allowlist_entries_are_parsed_and_validated_at_startup() {
    assert!(AllowedPrograms::parse("").expect("empty list").is_empty());
    assert!(AllowedPrograms::parse("  ").expect("blank list").is_empty());
    assert_eq!(
        AllowedPrograms::parse("git, cargo")
            .expect("names")
            .describe(),
        "git, cargo"
    );
    for invalid in ["git,,cargo", "tools/git", r"tools\git", ","] {
        assert!(
            AllowedPrograms::parse(invalid).is_err(),
            "{invalid} must be rejected"
        );
    }
}

/// The allowlist is checked after resolution against the canonical executable, so renaming a
/// copy to a name that is not on the list gains nothing.
#[test]
fn a_renamed_copy_is_denied_while_the_listed_name_is_permitted() {
    let fixture = tempfile::tempdir().expect("fixture");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let source = std::env::current_exe().expect("test executable");
    #[cfg(windows)]
    let (permitted_name, renamed_name) = ("allowedprobe.exe", "renamedprobe.exe");
    #[cfg(unix)]
    let (permitted_name, renamed_name) = ("allowedprobe", "renamedprobe");
    for name in [permitted_name, renamed_name] {
        std::fs::copy(&source, fixture.path().join(name)).expect("copy probe");
    }
    let resolver = ProcessResolver::for_tests(vec![fixture.path().to_owned()]);
    let allowed = allowlist(&["allowedprobe"]);
    let call = |program: &str| {
        let mut probe = request(program.to_owned());
        probe.args = vec!["--version".to_owned()];
        execute(
            &root,
            &resolver,
            &allowed,
            &probe,
            Duration::from_secs(10),
            &CancellationToken::new(),
        )
    };

    assert!(call("allowedprobe").is_ok());
    assert!(matches!(
        call("renamedprobe"),
        Err(ProcessError::NotPermitted(_))
    ));
}

#[test]
fn an_absolute_allowlist_entry_pins_one_executable() {
    let fixture = tempfile::tempdir().expect("fixture");
    let other = tempfile::tempdir().expect("second fixture");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let source = std::env::current_exe().expect("test executable");
    #[cfg(windows)]
    let name = "pinnedprobe.exe";
    #[cfg(unix)]
    let name = "pinnedprobe";
    let pinned = fixture.path().join(name);
    std::fs::copy(&source, &pinned).expect("copy pinned probe");
    std::fs::copy(&source, other.path().join(name)).expect("copy impostor");
    let allowed = allowlist(&[pinned.to_string_lossy().as_ref()]);
    let call = |directory: &std::path::Path| {
        let mut probe = request("pinnedprobe".to_owned());
        probe.args = vec!["--version".to_owned()];
        execute(
            &root,
            &ProcessResolver::for_tests(vec![directory.to_owned()]),
            &allowed,
            &probe,
            Duration::from_secs(10),
            &CancellationToken::new(),
        )
    };

    assert!(call(fixture.path()).is_ok());
    assert!(matches!(
        call(other.path()),
        Err(ProcessError::NotPermitted(_))
    ));
}

#[test]
fn a_bare_name_allows_either_the_invocation_alias_or_the_canonical_target() {
    let proxy = resolved("toolchain/cargo", "toolchain/rustup");

    assert!(allowlist(&["cargo"]).permits(&proxy));
    assert!(allowlist(&["rustup"]).permits(&proxy));
    assert!(!allowlist(&["unrelated"]).permits(&proxy));
}

#[test]
fn a_bare_alias_explicitly_allows_an_arbitrary_canonical_target() {
    let aliased = resolved("bin/reviewer-approved", "elsewhere/arbitrary-target");

    assert!(allowlist(&["reviewer-approved"]).permits(&aliased));
}

#[test]
fn an_absolute_entry_ignores_the_invocation_alias_and_pins_only_canonical_identity() {
    let fixture = tempfile::tempdir().expect("fixture");
    let source = std::env::current_exe().expect("test executable");
    let pinned = fixture.path().join(if cfg!(windows) {
        "pinned.exe"
    } else {
        "pinned"
    });
    let copy = fixture
        .path()
        .join(if cfg!(windows) { "copy.exe" } else { "copy" });
    std::fs::copy(&source, &pinned).expect("pinned copy");
    std::fs::copy(&source, &copy).expect("other copy");
    let pinned = std::fs::canonicalize(pinned).expect("canonical pinned copy");
    let copy = std::fs::canonicalize(copy).expect("canonical other copy");
    let allowed = allowlist(&[pinned.to_string_lossy().as_ref()]);

    assert!(allowed.permits(&resolved("another-alias", pinned.clone())));
    assert!(!allowed.permits(&resolved(pinned, copy)));
}

#[cfg(windows)]
#[test]
fn windows_allowlist_names_are_ascii_case_insensitive() {
    let allowed = allowlist(&["GiT"]);
    assert!(allowed.permits(&resolved(r"C:\tools\git.exe", r"C:\tools\git.exe")));
    assert!(allowed.permits(&resolved(r"C:\tools\GIT.EXE", r"C:\tools\GIT.EXE")));
    assert!(!allowed.permits(&resolved(r"C:\tools\gitk.exe", r"C:\tools\gitk.exe")));
}

/// The resolver caches successful lookups, so the allowlist must be consulted per call rather
/// than folded into the cached entry.
#[test]
fn a_cached_resolution_is_still_checked_against_the_allowlist() {
    let fixture = tempfile::tempdir().expect("fixture");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let source = std::env::current_exe().expect("test executable");
    #[cfg(windows)]
    let name = "cachedallowprobe.exe";
    #[cfg(unix)]
    let name = "cachedallowprobe";
    std::fs::copy(&source, fixture.path().join(name)).expect("copy probe");
    let resolver = ProcessResolver::for_tests(vec![fixture.path().to_owned()]);
    let call = |allowed: &AllowedPrograms| {
        let mut probe = request("cachedallowprobe".to_owned());
        probe.args = vec!["--version".to_owned()];
        execute(
            &root,
            &resolver,
            allowed,
            &probe,
            Duration::from_secs(10),
            &CancellationToken::new(),
        )
    };

    assert!(call(&allowlist(&["cachedallowprobe"])).is_ok());
    assert!(matches!(
        call(&AllowedPrograms::default()),
        Err(ProcessError::NotPermitted(_))
    ));
}
