use super::*;

fn available_bash() -> Option<Arc<BashRuntime>> {
    BashLocator::capture()
        .resolve(&CancellationToken::new())
        .ok()
}

/// Flavor-matching argv for driving the probe execution helpers against any discovered
/// runtime, including a busybox dispatcher reached through `AGENTSHIM_BASH`.
fn probe_invocation<'a>(runtime: &BashRuntime, script: &'a str) -> Vec<&'a str> {
    if runtime.busybox_dispatch {
        vec!["sh", "-c", script]
    } else {
        vec!["-c", script]
    }
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
    let missing_path = std::env::temp_dir().join("agentshim-definitely-missing-bash");
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
fn capture_with_override_uses_the_explicit_path() {
    let Some(runtime) = available_bash() else {
        return;
    };
    let valid =
        BashLocator::capture_with_override(Some(runtime.executable.clone().into_os_string()));
    assert!(valid.resolve(&CancellationToken::new()).is_ok());

    let missing = std::env::temp_dir().join("agentshim-definitely-missing-bash");
    let invalid = BashLocator::capture_with_override(Some(missing.into_os_string()));
    let error = invalid
        .resolve(&CancellationToken::new())
        .expect_err("explicit override must be used even when ambient AGENTSHIM_BASH is unset");
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
    let args = probe_invocation(&runtime, "head -c 65537 /dev/zero | tr '\\0' x");
    let output = probe_output(
        &runtime.executable,
        &args,
        runtime.path.as_deref(),
        &budget,
        &CancellationToken::new(),
    )
    .expect("probe execution");

    assert!(output.is_none());
}

#[test]
fn successful_candidate_uses_one_probe_and_warm_resolves_use_none() {
    let Some(runtime) = available_bash() else {
        return;
    };
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let locator = BashLocator::for_tests(
        Some(runtime.executable.clone().into_os_string()),
        Vec::new(),
        std::env::var_os("PATH").unwrap_or_default(),
    )
    .with_probe_counter(Arc::clone(&calls));

    assert!(locator.resolve(&CancellationToken::new()).is_ok());
    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert!(locator.resolve(&CancellationToken::new()).is_ok());
    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
}

#[test]
fn probe_parser_requires_the_version_marker_on_the_first_line() {
    for output in [
        "GNU bash, version 5.2.37\nC.UTF-8\n",
        "C.UTF-8\nAGENTSHIM_BASH_PROBE_V1:5.2.37\n",
        "AGENTSHIM_BASH_PROBE_V1:\nC.UTF-8\n",
        "AGENTSHIM_BASH_PROBE_V1:\n",
    ] {
        assert!(parse_probe_output(output).is_none(), "accepted {output:?}");
    }
}

#[test]
fn probe_parser_selects_preferred_locale_or_fallback() {
    assert_eq!(
        parse_probe_output("AGENTSHIM_BASH_PROBE_V1:5.2.37\nC.utf8\n"),
        Some((ShellFlavor::Bash, PREFERRED_LOCALE.to_owned()))
    );
    assert_eq!(
        parse_probe_output("AGENTSHIM_BASH_PROBE_V1:5.2.37\nC\nPOSIX\n"),
        Some((ShellFlavor::Bash, FALLBACK_LOCALE.to_owned()))
    );
    assert_eq!(
        parse_probe_output("AGENTSHIM_BASH_PROBE_V1:5.2.37\n"),
        Some((ShellFlavor::Bash, FALLBACK_LOCALE.to_owned())),
        "a missing locale command must not reject a valid Bash"
    );
    assert_eq!(
        parse_probe_output("AGENTSHIM_BASH_PROBE_V1:busybox-ash\n"),
        Some((ShellFlavor::Ash, FALLBACK_LOCALE.to_owned()))
    );
}

#[test]
fn busybox_dispatch_matches_prefixed_executable_names() {
    for name in [
        "busybox",
        "busybox.exe",
        "busybox64.exe",
        "busybox64u.exe",
        "BusyBox64.EXE",
        "busybox-helper.exe",
    ] {
        assert!(
            busybox_dispatch(Path::new(name)),
            "{name} is a dispatcher name"
        );
    }
    for name in ["bash.exe", "sh.exe", "ash.exe", "sh", "my-busybox.exe"] {
        assert!(
            !busybox_dispatch(Path::new(name)),
            "{name} is not a dispatcher name"
        );
    }
}

#[test]
fn probe_args_carry_the_sh_prefix_only_for_a_dispatcher() {
    assert_eq!(probe_args(true), &["sh", "-c", PROBE_SCRIPT][..]);
    assert_eq!(probe_args(false), &["-c", PROBE_SCRIPT][..]);
    for args in [probe_args(true), probe_args(false)] {
        assert!(
            args.iter().all(|arg| !arg.starts_with("--")),
            "the probe must not pass long options: {args:?}"
        );
    }
}

#[test]
fn the_probe_environment_strips_an_inherited_bash_version() {
    let plan = probe_environment(Some("/toolchain/bin"));

    for name in ["BASH_ENV", "ENV", "BASH_VERSION"] {
        assert!(
            plan.removed.iter().any(|removed| removed == name),
            "{name} must be removed from the probe environment"
        );
    }
    assert_eq!(
        plan.overrides,
        vec![
            ("LC_ALL".to_owned(), "C".to_owned()),
            ("PATH".to_owned(), "/toolchain/bin".to_owned()),
        ]
    );
}

#[test]
fn the_probe_environment_neutralizes_host_locale() {
    let plan = probe_environment(None);
    assert_eq!(plan.overrides, vec![("LC_ALL".to_owned(), "C".to_owned())]);
}

#[test]
fn probe_timeout_terminates_the_process_tree() {
    let Some(runtime) = available_bash() else {
        return;
    };
    let budget = Budget::new(Duration::from_millis(100));
    let args = probe_invocation(&runtime, "sleep 30");
    let started = Instant::now();
    let output = probe_output(
        &runtime.executable,
        &args,
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
    let args = probe_invocation(
        &runtime,
        "(sleep 1; printf late > delayed-marker) & printf 'probe complete\\n'",
    );
    let started = Instant::now();
    let output = probe_output_in(
        &runtime.executable,
        &args,
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
        let args = probe_invocation(&runtime, "(sleep 1; printf late > delayed-marker) & wait");
        probe_output_in(
            &runtime.executable,
            &args,
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

#[test]
fn search_path_returns_every_hit_in_order() {
    let fixture = tempfile::tempdir().expect("fixture");
    let first = fixture.path().join("first");
    let second = fixture.path().join("second");
    std::fs::create_dir_all(&first).expect("first directory");
    std::fs::create_dir_all(&second).expect("second directory");
    std::fs::write(first.join("bash.exe"), b"").expect("first hit");
    std::fs::write(second.join("bash.exe"), b"").expect("second hit");
    let inherited = std::env::join_paths([&first, &second]).expect("joined PATH");

    let hits = search_path("bash.exe", &inherited);

    assert_eq!(hits, vec![first.join("bash.exe"), second.join("bash.exe")]);
}

#[cfg(windows)]
#[test]
fn system_root_exclusion_respects_component_boundaries_and_slash_forms() {
    let root = OsString::from("C:\\Windows");
    for excluded in [
        r"C:\Windows\System32\bash.exe",
        r"C:/Windows/System32/bash.exe",
        r"c:\WINDOWS\bash.exe",
        r"C:\Windows",
        r"C:\Windows\",
    ] {
        assert!(
            is_excluded_with(Path::new(excluded), Some(&root)),
            "{excluded} must be excluded"
        );
    }
    for kept in [
        r"C:\Windows-Tools\bash.exe",
        r"C:\Windowsity\bash.exe",
        r"D:\Windows\System32\bash.exe",
        r"C:\Users\me\Git\usr\bin\bash.exe",
    ] {
        assert!(
            !is_excluded_with(Path::new(kept), Some(&root)),
            "{kept} must not be excluded"
        );
    }
    assert!(!is_excluded_with(
        Path::new(r"C:\Windows\System32\bash.exe"),
        None
    ));
    assert!(!is_excluded_with(
        Path::new(r"C:\Windows\System32\bash.exe"),
        Some(&OsString::new())
    ));
}

#[cfg(windows)]
#[test]
fn windowsapps_exclusion_matches_a_component_in_either_slash_form() {
    let root = OsString::from("C:\\Windows");
    for excluded in [
        r"C:\Users\me\AppData\Local\Microsoft\WindowsApps\bash.exe",
        r"C:\Users\me\AppData\Local\Microsoft\windowsapps\bash.exe",
        r"C:/Users/me/AppData/Local/Microsoft/WindowsApps/bash.exe",
        r"D:\Tools\WindowsApps\bash.exe",
    ] {
        assert!(
            is_excluded_with(Path::new(excluded), Some(&root)),
            "{excluded} must be excluded"
        );
    }
    assert!(
        !is_excluded_with(Path::new(r"C:\Tools\windows-apps\bash.exe"), Some(&root)),
        "a lookalike component is not the Store alias directory"
    );
}

#[cfg(windows)]
#[test]
fn an_excluded_override_is_rejected_without_entering_wsl() {
    for override_path in [
        r"C:\Windows\System32\bash.exe",
        r"C:/Windows/System32/bash.exe",
        r"C:\Users\me\AppData\Local\Microsoft\WindowsApps\bash.exe",
    ] {
        let locator = BashLocator::for_tests(
            Some(OsString::from(override_path)),
            Vec::new(),
            OsString::new(),
        );
        let error = locator
            .resolve(&CancellationToken::new())
            .expect_err("an excluded override must not be used");

        let LocateError::Unavailable(message) = error else {
            panic!("expected an unavailable error for {override_path}");
        };
        assert!(
            message.contains("WSL launcher"),
            "the message must name the WSL launcher: {message}"
        );
    }
}

#[cfg(windows)]
#[test]
fn a_later_path_hit_is_used_when_the_first_bash_exe_is_excluded() {
    let Some(runtime) = available_bash() else {
        return;
    };
    let fixture = tempfile::tempdir().expect("fixture");
    let store_alias = fixture.path().join("Microsoft").join("WindowsApps");
    std::fs::create_dir_all(&store_alias).expect("Store alias directory");
    std::fs::write(store_alias.join("bash.exe"), b"").expect("alias hit");
    let real_directory = runtime
        .executable
        .parent()
        .expect("bash parent directory")
        .to_owned();
    let inherited = std::env::join_paths([&store_alias, &real_directory]).expect("joined PATH");

    let candidates = search_path("bash.exe", &inherited);
    assert_eq!(candidates.len(), 2, "both hits must be collected");
    let locator = BashLocator::for_tests(None, candidates, inherited);
    let resolved = locator
        .resolve(&CancellationToken::new())
        .expect("the excluded first hit falls through to the later one");

    assert_eq!(resolved.executable, runtime.executable);
}

#[cfg(windows)]
#[test]
fn an_arm64_git_layout_prefixes_clangarm64_bin() {
    let fixture = tempfile::tempdir().expect("fixture");
    let own = fixture.path().join("usr").join("bin");
    let arm64 = fixture.path().join("clangarm64").join("bin");
    std::fs::create_dir_all(&own).expect("usr/bin");
    std::fs::create_dir_all(&arm64).expect("clangarm64/bin");
    std::fs::write(own.join("bash.exe"), b"").expect("layout bash");

    let path = toolchain_path(&own.join("bash.exe"), &OsString::new(), false)
        .expect("a bin layout must yield a toolchain PATH");
    let entries = std::env::split_paths(&path).collect::<Vec<_>>();

    assert!(
        entries.contains(&arm64),
        "clangarm64/bin is missing: {path}"
    );
    assert_eq!(entries.last(), Some(&own));
}

#[cfg(windows)]
#[test]
fn a_busybox_dispatcher_gets_only_its_own_directory_ahead_of_the_inherited_path() {
    let fixture = tempfile::tempdir().expect("fixture");
    let own = fixture.path().join("usr").join("bin");
    let mingw = fixture.path().join("mingw64").join("bin");
    std::fs::create_dir_all(&own).expect("usr/bin");
    std::fs::create_dir_all(&mingw).expect("mingw64/bin");
    std::fs::write(own.join("busybox64u.exe"), b"").expect("dispatcher binary");
    let inherited = std::env::join_paths([fixture.path()]).expect("joined PATH");

    let dispatcher = toolchain_path(&own.join("busybox64u.exe"), &inherited, true)
        .expect("a dispatcher must yield a toolchain PATH");
    let bash = toolchain_path(&own.join("bash.exe"), &inherited, false)
        .expect("a bin layout must yield a toolchain PATH");
    let dispatcher_entries = std::env::split_paths(&dispatcher).collect::<Vec<_>>();
    let bash_entries = std::env::split_paths(&bash).collect::<Vec<_>>();

    assert_eq!(
        dispatcher_entries.first(),
        Some(&own),
        "the own directory must lead: {dispatcher}"
    );
    assert!(
        !dispatcher_entries.contains(&mingw),
        "a dispatcher must not probe MSYS2 toolchain directories: {dispatcher}"
    );
    assert!(
        bash_entries.contains(&mingw),
        "the MSYS2 layout keeps its lookup: {bash}"
    );
}

#[cfg(windows)]
#[test]
fn candidates_place_busybox_after_every_full_bash_source() {
    let fixture = tempfile::tempdir().expect("fixture");
    let bin = fixture.path().join("bin");
    std::fs::create_dir_all(&bin).expect("bin");
    for name in ["git.exe", "bash.exe", "busybox64u.exe", "busybox64.exe"] {
        std::fs::write(bin.join(name), b"").expect("candidate file");
    }
    let inherited = std::env::join_paths([&bin]).expect("joined PATH");
    let program_files = Some(OsString::from(fixture.path()));
    let local_app_data = Some(OsString::from(fixture.path()));

    let candidates = candidates_with(
        &[program_files.clone(), program_files],
        local_app_data.as_deref(),
        &inherited,
    );

    let position = |candidate: &Path| {
        candidates
            .iter()
            .position(|entry| entry == candidate)
            .unwrap_or_else(|| panic!("{candidate:?} is missing from {candidates:?}"))
    };
    let path_bash = position(&bin.join("bash.exe"));
    let fixed_unicode = position(&fixture.path().join("busybox-w32").join("busybox64u.exe"));
    let fixed_ansi = position(&fixture.path().join("busybox-w32").join("busybox64.exe"));
    let path_unicode = position(&bin.join("busybox64u.exe"));

    assert!(
        path_bash < fixed_unicode,
        "a PATH bash.exe must precede the fixed busybox locations: {candidates:?}"
    );
    assert!(
        fixed_unicode < fixed_ansi,
        "the Unicode build must precede the ANSI build: {candidates:?}"
    );
    assert!(
        fixed_ansi < path_unicode,
        "fixed busybox locations must precede PATH busybox hits: {candidates:?}"
    );
}
