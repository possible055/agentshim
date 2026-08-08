#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::{fs, os::unix::process::CommandExt};
    #[cfg(windows)]
    use std::{env, process::Command, thread};
    #[cfg(any(unix, windows))]
    use std::{sync::Arc, time::Duration};

    #[cfg(any(unix, windows))]
    use crate::path::RepositoryRoot;

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

    #[cfg(windows)]
    #[test]
    fn powershell_command_evaluation_switches_are_classified_conservatively() {
        for denied in [
            "-Command",
            "-c",
            "-command:Get-Process",
            "-CommandWithArgs",
            "-cwa",
            "-EncodedCommand",
            "-e",
            "-ec",
            "-enc",
            "-encodedcommand=payload",
        ] {
            assert!(
                is_powershell_command_evaluation_arg(denied),
                "{denied} must be rejected"
            );
        }
        for allowed in [
            "-ConfigurationName",
            "-EncodedArguments",
            "-ExecutionPolicy",
            "-File",
            "-NoProfile",
        ] {
            assert!(
                !is_powershell_command_evaluation_arg(allowed),
                "{allowed} is not a command-evaluation switch"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn resolver_ignores_empty_path_and_requires_executable_regular_file() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = tempfile::tempdir().expect("fixture");
        let executable = fixture.path().join("probe");
        fs::write(&executable, "#!/bin/sh\nexit 0\n").expect("write probe");
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).expect("chmod");
        let resolver = ProcessResolver::for_tests(vec![fixture.path().to_owned()]);
        let program = resolver.resolve("probe", fixture.path()).expect("resolve");
        let executable = fs::canonicalize(executable).expect("canonical");
        assert_eq!(program.absolute, executable);
        assert_eq!(program.executable, executable);
        assert!(resolver.resolve("probe arg", fixture.path()).is_err());
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
            "tools::process::tests::unix_multicall_argv0_child_fixture".to_owned(),
            "--nocapture".to_owned(),
        ];
        proxy_request
            .env
            .insert("CODEXSHIM_MULTICALL_FIXTURE".to_owned(), "child".to_owned());

        let output = execute(
            &root,
            &resolver,
            &proxy_request,
            Duration::from_secs(5),
            &CancellationToken::new(),
        )
        .expect("multicall proxy");

        assert!(output.contains(&format!("Resolved program: {}", proxy.display())));
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
    fn invalid_utf8_is_escaped_across_valid_spans() {
        let (rendered, invalid) = escape_invalid_utf8(b"a\xF0\x9F\x92\xA9b\xFFc\xE2\x82");
        assert_eq!(rendered, "a💩b\\xFFc\\xE2\\x82");
        assert_eq!(invalid, 3);

        let mut capture = Capture::new();
        capture.push(b"a\xF0\x9F");
        capture.push(b"\x92\xA9b\xFF");
        let rendered = capture.render();
        assert_eq!(rendered.text, "a💩b\\xFF");
        assert_eq!(rendered.invalid_bytes, 1);
    }

    #[test]
    fn timeout_report_is_bounded_and_preserves_required_diagnostics() {
        let mut stdout = Capture::new();
        stdout.push(&vec![b'o'; crate::output::MODEL_BYTE_LIMIT * 2]);
        let mut stderr = Capture::new();
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

    #[test]
    fn capture_keeps_bounded_head_and_tail_while_counting_all_bytes() {
        let bytes = vec![b'x'; CAPTURE_HEAD_BYTES + CAPTURE_TAIL_BYTES + 17];
        let mut capture = Capture::new();
        capture.push(&bytes);
        assert_eq!(capture.bytes_read, bytes.len());
        assert_eq!(capture.retained(), CAPTURE_HEAD_BYTES + CAPTURE_TAIL_BYTES);
        assert_eq!(capture.dropped(), 17);
    }

    #[cfg(unix)]
    fn execute_unix(request: &ProcessRequest) -> Result<String, ProcessError> {
        let fixture = tempfile::tempdir().expect("fixture");
        let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
        execute(
            &root,
            &ProcessResolver::capture(),
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
            &absolute,
            Duration::from_secs(5),
            &CancellationToken::new(),
        )
        .expect("absolute cwd outside root");
        assert!(output.contains(&format!("Cwd: {}", outside.path().display())));

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
        assert!(output.contains("dropped="));
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
        let pid_file =
            std::env::var_os("CODEXSHIM_SESSION_ESCAPE_PID_FILE").expect("helper PID file");
        let mut command = std::process::Command::new(
            std::env::current_exe().expect("test executable"),
        );
        command
            .args([
                "--exact",
                "tools::process::tests::unix_session_escape_helper_fixture",
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
        let pid_file =
            std::env::var_os("CODEXSHIM_SESSION_ESCAPE_PID_FILE").expect("helper PID file");
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
            "tools::process::tests::unix_session_escape_parent_fixture".to_owned(),
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
                "tools::process::tests::windows_grandchild_child_fixture",
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
                "tools::process::tests::windows_grandchild_child_fixture",
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
            System::Threading::{
                GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
            },
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
            "tools::process::tests::windows_lingering_grandchild_parent_fixture".to_owned(),
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
            "tools::process::tests::windows_grandchild_parent_fixture".to_owned(),
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
}
