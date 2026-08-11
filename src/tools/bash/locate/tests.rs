use super::*;

fn available_bash() -> Option<Arc<BashRuntime>> {
    BashLocator::capture()
        .resolve(&CancellationToken::new())
        .ok()
}

#[test]
fn locator_instances_keep_captured_inputs_independent() {
    let Some(runtime) = available_bash() else {
        return;
    };
    let valid = BashLocator::for_tests(
        Some(runtime.executable.clone().into_os_string()),
        Vec::new(),
        std::env::var_os("PATH").unwrap_or_default(),
    );
    let missing_path = std::env::temp_dir().join("codexshim-definitely-missing-bash");
    let invalid = BashLocator::for_tests(
        Some(missing_path.clone().into_os_string()),
        Vec::new(),
        OsString::new(),
    );

    assert!(valid.resolve(&CancellationToken::new()).is_ok());
    let error = invalid
        .resolve(&CancellationToken::new())
        .expect_err("the second locator has different captured inputs");
    assert!(matches!(error, LocateError::Unavailable(_)));
}

#[test]
fn cancelled_probe_returns_to_empty_and_can_be_retried() {
    let Some(runtime) = available_bash() else {
        return;
    };
    let gate = Arc::new(TestProbeGate::new(Arc::clone(&runtime)));
    let locator = BashLocator::for_tests(
        Some(runtime.executable.clone().into_os_string()),
        Vec::new(),
        std::env::var_os("PATH").unwrap_or_default(),
    )
    .with_probe_gate(Arc::clone(&gate));
    let cancellation = CancellationToken::new();
    let worker_locator = locator.clone();
    let worker_cancellation = cancellation.clone();
    let worker = std::thread::spawn(move || worker_locator.resolve(&worker_cancellation));
    gate.entered.wait();
    cancellation.cancel();

    assert!(matches!(
        worker.join().expect("probe worker"),
        Err(LocateError::Cancelled)
    ));
    assert!(locator.resolve(&CancellationToken::new()).is_ok());
}

#[test]
fn a_waiter_can_cancel_without_cancelling_the_shared_probe() {
    let Some(runtime) = available_bash() else {
        return;
    };
    let gate = Arc::new(TestProbeGate::new(Arc::clone(&runtime)));
    let locator = BashLocator::for_tests(
        Some(runtime.executable.clone().into_os_string()),
        Vec::new(),
        std::env::var_os("PATH").unwrap_or_default(),
    )
    .with_probe_gate(Arc::clone(&gate));
    let owner_locator = locator.clone();
    let owner = std::thread::spawn(move || owner_locator.resolve(&CancellationToken::new()));
    gate.entered.wait();
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(matches!(
        locator.resolve(&cancelled),
        Err(LocateError::Cancelled)
    ));
    gate.release();
    assert!(owner.join().expect("owner").is_ok());
    assert!(locator.resolve(&CancellationToken::new()).is_ok());
}

#[test]
fn owner_deadline_resets_the_locator_for_a_later_probe() {
    let Some(runtime) = available_bash() else {
        return;
    };
    let gate = Arc::new(TestProbeGate::new(Arc::clone(&runtime)));
    let locator = BashLocator::for_tests(
        Some(runtime.executable.clone().into_os_string()),
        Vec::new(),
        std::env::var_os("PATH").unwrap_or_default(),
    )
    .with_probe_gate(Arc::clone(&gate));
    let worker_locator = locator.clone();
    let worker = std::thread::spawn(move || {
        worker_locator.resolve_before(
            &CancellationToken::new(),
            Instant::now() + Duration::from_millis(100),
        )
    });
    gate.entered.wait();

    assert!(matches!(
        worker.join().expect("probe owner"),
        Err(LocateError::TimedOut)
    ));
    assert!(locator.resolve(&CancellationToken::new()).is_ok());
}

#[test]
fn waiter_deadline_does_not_cancel_the_shared_probe_owner() {
    let Some(runtime) = available_bash() else {
        return;
    };
    let gate = Arc::new(TestProbeGate::new(Arc::clone(&runtime)));
    let locator = BashLocator::for_tests(
        Some(runtime.executable.clone().into_os_string()),
        Vec::new(),
        std::env::var_os("PATH").unwrap_or_default(),
    )
    .with_probe_gate(Arc::clone(&gate));
    let owner_locator = locator.clone();
    let owner = std::thread::spawn(move || owner_locator.resolve(&CancellationToken::new()));
    gate.entered.wait();

    assert!(matches!(
        locator.resolve_before(
            &CancellationToken::new(),
            Instant::now() + Duration::from_millis(50)
        ),
        Err(LocateError::TimedOut)
    ));
    gate.release();
    assert!(owner.join().expect("probe owner").is_ok());
    assert!(locator.resolve(&CancellationToken::new()).is_ok());
}

#[test]
fn probe_rejects_output_over_sixty_four_kibibytes() {
    let Some(runtime) = available_bash() else {
        return;
    };
    let budget = Budget::new(Duration::from_secs(5));
    let output = probe_output(
        &runtime.executable,
        &[
            "--noprofile",
            "--norc",
            "-c",
            "head -c 65537 /dev/zero | tr '\\0' x",
        ],
        runtime.path.as_deref(),
        &budget,
        &CancellationToken::new(),
    )
    .expect("probe execution");

    assert!(output.is_none());
}

#[test]
fn probe_timeout_terminates_the_process_tree() {
    let Some(runtime) = available_bash() else {
        return;
    };
    let budget = Budget::new(Duration::from_millis(100));
    let started = Instant::now();
    let output = probe_output(
        &runtime.executable,
        &["--noprofile", "--norc", "-c", "sleep 30"],
        runtime.path.as_deref(),
        &budget,
        &CancellationToken::new(),
    )
    .expect("probe execution");

    assert!(output.is_none());
    assert!(started.elapsed() < Duration::from_secs(6));
}

#[test]
fn successful_primary_with_a_pipe_holding_descendant_is_bounded() {
    let Some(runtime) = available_bash() else {
        return;
    };
    let fixture = tempfile::tempdir().expect("fixture");
    let budget = Budget::new(Duration::from_secs(5));
    let started = Instant::now();
    let output = probe_output_in(
        &runtime.executable,
        &[
            "--noprofile",
            "--norc",
            "-c",
            "(sleep 1; printf late > delayed-marker) & printf 'probe complete\\n'",
        ],
        runtime.path.as_deref(),
        &budget,
        &CancellationToken::new(),
        fixture.path(),
    )
    .expect("probe execution")
    .expect("successful probe output");

    assert!(output.contains("probe complete"));
    assert!(started.elapsed() < Duration::from_secs(6));
    std::thread::sleep(Duration::from_millis(1_250));
    assert!(
        !fixture.path().join("delayed-marker").exists(),
        "probe descendant survived process-tree cleanup"
    );
}

#[test]
fn cancelling_a_running_probe_terminates_its_process_tree() {
    let Some(runtime) = available_bash() else {
        return;
    };
    let fixture = tempfile::tempdir().expect("fixture");
    let cwd = fixture.path().to_owned();
    let cancellation = CancellationToken::new();
    let worker_cancellation = cancellation.clone();
    let worker = std::thread::spawn(move || {
        let budget = Budget::new(Duration::from_secs(5));
        probe_output_in(
            &runtime.executable,
            &[
                "--noprofile",
                "--norc",
                "-c",
                "(sleep 1; printf late > delayed-marker) & wait",
            ],
            runtime.path.as_deref(),
            &budget,
            &worker_cancellation,
            &cwd,
        )
    });
    std::thread::sleep(Duration::from_millis(100));
    cancellation.cancel();

    assert!(matches!(
        worker.join().expect("probe worker"),
        Err(ProbeError::Cancelled)
    ));
    std::thread::sleep(Duration::from_millis(1_250));
    assert!(
        !fixture.path().join("delayed-marker").exists(),
        "cancelled probe descendant survived process-tree cleanup"
    );
}

#[cfg(windows)]
#[test]
fn a_git_layout_gains_its_own_toolchain_directories_ahead_of_the_inherited_path() {
    let Some(runtime) = available_bash() else {
        return;
    };
    let own = runtime.executable.parent().expect("bash parent directory");
    if !own
        .file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case("bin"))
    {
        return;
    }
    let path = runtime
        .path
        .as_deref()
        .expect("a bin layout must yield a toolchain PATH");
    let first = std::env::split_paths(path).next().expect("a first entry");

    assert!(std::env::split_paths(path).any(|entry| entry == own));
    assert!(first.is_dir());
    for inherited in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        if !inherited.as_os_str().is_empty() {
            assert!(std::env::split_paths(path).any(|entry| entry == inherited));
        }
    }
}

#[cfg(windows)]
#[test]
fn an_unrecognised_layout_leaves_the_inherited_path_alone() {
    assert!(toolchain_path(Path::new(r"C:\tools\bash.exe"), OsStr::new("C:\\Windows")).is_none());
}
