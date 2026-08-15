use super::*;

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
fn empty_native_success_is_only_the_exit_code() {
    let compact = completed_output(b"", b"", "0").text;
    assert_eq!(compact, "Exit code: 0");
}

#[cfg(windows)]
#[test]
fn non_native_success_keeps_resolution_diagnostics() {
    let completed = CompletedProcess {
        resolved: ResolvedProgram {
            absolute: PathBuf::from("script.cmd"),
            executable: PathBuf::from("cmd.exe"),
            launcher: Launcher::CmdCompat,
        },
        cwd: PathBuf::from("workspace"),
        exit: "0".to_owned(),
        duration: Duration::from_millis(1),
        stdout: Capture::new(0),
        stderr: Capture::new(0),
    };

    let output = render_completed(&completed, &CancellationToken::new()).expect("completion");
    assert!(output.contains("Resolved program: script.cmd"));
    assert!(output.contains("Launcher: cmd-compat"));
    assert!(output.ends_with("Exit code: 0"));
    assert!(!output.contains("Cwd:"));
    assert!(!output.contains("Duration ms:"));
}

#[test]
fn dynamic_burst_ceiling_keeps_process_completion_metadata() {
    let mut stdout = Capture::new(100_000);
    stdout.push(" x".repeat(40_000).as_bytes());
    let stderr = Capture::new(0);
    let gate = crate::output::BurstOutputGate::new(512);
    let budget = crate::output::CallOutputBudget::new(
        crate::output::OutputTokenGate::load_shared().expect("token gate"),
        gate.begin_call(),
    );
    let output = render_completed_with_budget(
        &CompletedProcess {
            resolved: ResolvedProgram {
                absolute: PathBuf::from("tool"),
                executable: PathBuf::from("tool"),
                launcher: Launcher::Native,
            },
            cwd: "workspace".into(),
            exit: "7".to_owned(),
            duration: Duration::from_millis(42),
            stdout,
            stderr,
        },
        &CancellationToken::new(),
        &budget,
    )
    .expect("bounded completion summary");
    assert!(output.contains("Exit code: 7"));
    assert!(output.contains("Duration ms: 42"));
    assert!(output.contains("Stdout: total="));
    assert!(output.contains("omitted="));
    assert!(!output.contains("Partial:"));
    assert!(!output.contains("Complete."));
    assert!(output.fits_call_budget(&budget, &CancellationToken::new()));
    assert!(output.fits_model_budget(&CancellationToken::new()));
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
        "tools::run_program::tests::output::unix_multicall_argv0_child_fixture".to_owned(),
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

    assert!(output.contains("multicall argv0: cargo"));
    assert!(output.contains("Exit code: 0"));
    assert!(!output.contains("Resolved program:"));
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
        &CancellationToken::new(),
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
            "7",
            &program,
            &cwd,
        );

        assert!(
            output.fits_budget(),
            "header near the limit broke the budget"
        );
        assert!(output.text.contains("...[path truncated]..."));
        assert!(output.text.contains("Resolved program: "));
        assert!(output.text.contains("Exit code: 7"));
        assert!(!output.text.contains("Complete."));
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
fn token_dense_process_output_keeps_head_tail_and_metadata() {
    let stdout = format!("HEAD\n{}\nTAIL\n", " x".repeat(20_000));
    let cancellation = CancellationToken::new();
    let output = completed_output(stdout.as_bytes(), b"stderr evidence\n", "7");

    assert!(output.fits_budget());
    assert!(output.fits_model_budget(&cancellation));
    assert!(output.contains("HEAD"));
    assert!(output.contains("TAIL"));
    assert!(output.contains("bytes omitted"));
    assert!(output.contains("Exit code: 7"));
    assert!(!output.contains("Stderr: total="));
    assert!(!output.contains("Complete."));
}

#[test]
fn single_stream_output_that_fits_is_returned_in_full() {
    let stdout = vec![b'x'; 8_908];

    let output = completed_output(&stdout, b"", "0");

    assert!(output.fits_budget());
    let stdout_text = output
        .text
        .split_once("--- stdout ---\n")
        .and_then(|(_, output)| output.split_once("\nExit code:"))
        .map(|(stdout, _)| stdout)
        .expect("stdout section");
    assert_eq!(stdout_text.as_bytes(), stdout);
    assert!(!output.text.contains("Stdout: total="));
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
    assert!(stdout_shown.abs_diff(stderr_shown) <= 1);
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
        assert!(!output.text.contains("Complete."));
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
    assert!(!output.text.contains("Stderr: total="));
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
        &CancellationToken::new(),
    )
    .expect("timeout report");
    let details = serde_json::to_value(&report.details).expect("timeout details");
    let structured =
        crate::output::tool_error_structure("resource_timeout", true, &report.text, Some(&details));

    assert!(crate::output::tool_result_fits_budget(
        &report.text,
        Some(&structured),
        true
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
        "tools::run_program::tests::output::interleaved_output_child_fixture".to_owned(),
        "--nocapture".to_owned(),
    ];
    interleaved.env.insert(
        "CODEXSHIM_INTERLEAVE_FIXTURE".to_owned(),
        "child".to_owned(),
    );

    let output = execute(
        &root,
        &ProcessResolver::capture(),
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
        "tools::run_program::tests::output::high_escaping_output_child_fixture".to_owned(),
        "--nocapture".to_owned(),
    ];
    high_output
        .env
        .insert("CODEXSHIM_OUTPUT_FIXTURE".to_owned(), "child".to_owned());

    let output = execute_output(
        &root,
        &ProcessResolver::capture(),
        &high_output,
        Duration::from_secs(10),
        &CancellationToken::new(),
    )
    .expect("high escaping output");

    assert!(output.fits_budget());
    assert!(output.text.contains("bytes omitted"));
    assert!(!output.text.contains("Complete."));
}
