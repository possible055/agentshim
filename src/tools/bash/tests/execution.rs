use super::*;

#[test]
fn omitted_msys_argument_conversion_uses_the_default_and_unknown_values_are_rejected() {
    let request: BashRequest =
        serde_json::from_str(r#"{"command":"true"}"#).expect("request without mode");
    assert_eq!(
        request.msys_argument_conversion,
        MsysArgumentConversion::Default
    );

    let error = serde_json::from_str::<BashRequest>(
        r#"{"command":"true","msys_argument_conversion":"unexpected"}"#,
    )
    .expect_err("unknown conversion mode");
    assert!(error.to_string().contains("unknown variant"), "{error}");
}

#[test]
fn validation_rejects_empty_commands_and_unsupported_detach_combinations() {
    let mut empty = request("");
    empty.command = String::new();
    assert!(matches!(empty.validate(), Err(ProcessError::Validation(_))));

    let mut without_log = request("true");
    without_log.detach = true;
    without_log.timeout_ms = None;
    assert!(matches!(
        without_log.validate(),
        Err(ProcessError::Validation(_))
    ));

    let mut with_timeout = request("true");
    with_timeout.detach = true;
    with_timeout.log_path = Some("build.log".to_owned());
    assert!(matches!(
        with_timeout.validate(),
        Err(ProcessError::Validation(_))
    ));

    let mut accepted = request("true");
    accepted.detach = true;
    accepted.timeout_ms = None;
    accepted.log_path = Some("build.log".to_owned());
    assert!(accepted.validate().is_ok());

    let mut logged = request("true");
    logged.log_path = Some("local/build.log".to_owned());
    assert!(matches!(
        logged.validate(),
        Err(ProcessError::Validation(_))
    ));

    let mut over_limit = request("true");
    over_limit.timeout_ms = Some(max_timeout_ms() + 1);
    assert!(matches!(
        over_limit.validate(),
        Err(ProcessError::Validation(_))
    ));

    let mut at_limit = request("true");
    at_limit.timeout_ms = Some(max_timeout_ms());
    assert!(at_limit.validate().is_ok());
}

fn runtime_fixture(path: Option<&str>) -> locate::BashRuntime {
    locate::BashRuntime {
        executable: std::path::PathBuf::from("/usr/bin/bash"),
        locale: "C.UTF-8".to_owned(),
        path: path.map(str::to_owned),
    }
}

#[test]
fn injected_environment_carries_the_probed_locale_and_no_colour_defaults() {
    let plan = environment(&runtime_fixture(None), MsysArgumentConversion::Default);
    let injected = |key: &str| {
        plan.injected
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
    };

    assert_eq!(injected("LANG").as_deref(), Some("C.UTF-8"));
    assert_eq!(injected("LC_ALL").as_deref(), Some("C.UTF-8"));
    assert_eq!(injected("PYTHONUNBUFFERED").as_deref(), Some("1"));
    assert_eq!(injected("GIT_EDITOR").as_deref(), Some("true"));
    assert_eq!(plan.injected.len(), BASH_ENVIRONMENT.len() + 2);
    assert_eq!(
        plan.removed,
        STRIPPED_INHERITED_ENV.map(str::to_owned).to_vec(),
        "BASH_ENV and ENV must not reach a --noprofile --norc shell"
    );
    assert!(plan.overrides.is_empty());
}

/// `PATH` has to override rather than inject: on Windows the inherited variable may be spelled
/// `Path`, and an injected duplicate would lose to it.
#[test]
fn a_probed_toolchain_path_replaces_the_inherited_one() {
    let plan = environment(
        &runtime_fixture(Some("/toolchain/bin")),
        MsysArgumentConversion::Default,
    );

    assert_eq!(
        plan.overrides,
        vec![("PATH".to_owned(), "/toolchain/bin".to_owned())]
    );
    assert!(plan.injected.iter().all(|(name, _)| name != "PATH"));
}

#[test]
fn the_default_msys_mode_does_not_change_the_inherited_conversion_environment() {
    let plan = environment(&runtime_fixture(None), MsysArgumentConversion::Default);

    assert!(
        plan.injected
            .iter()
            .all(|(name, _)| name != "MSYS2_ARG_CONV_EXCL")
    );
    assert!(
        plan.removed
            .iter()
            .all(|name| name != "MSYS2_ARG_CONV_EXCL")
    );
    assert!(
        plan.overrides
            .iter()
            .all(|(name, _)| name != "MSYS2_ARG_CONV_EXCL")
    );
}

#[cfg(windows)]
#[test]
fn disabled_msys_conversion_overrides_the_runtime_for_the_whole_bash_command() {
    let plan = environment(
        &runtime_fixture(Some("/toolchain/bin")),
        MsysArgumentConversion::Disabled,
    );

    assert_eq!(
        plan.overrides,
        vec![
            ("PATH".to_owned(), "/toolchain/bin".to_owned()),
            ("MSYS2_ARG_CONV_EXCL".to_owned(), "*".to_owned()),
        ]
    );
}

#[cfg(not(windows))]
#[test]
fn disabled_msys_conversion_is_a_noop_outside_windows() {
    let plan = environment(&runtime_fixture(None), MsysArgumentConversion::Disabled);

    assert!(plan.overrides.is_empty());
}

/// The failure this reproduces is a shell that cannot see its own coreutils: on Windows a
/// `PowerShell` `PATH` carries `Git\cmd` and `Git\mingw64\bin` but not `Git\usr\bin`, so `sleep`,
/// `grep`, and `sed` all exit 127.
#[test]
fn the_shell_can_run_the_utilities_that_ship_beside_it() {
    if !bash_is_available() {
        return;
    }
    let output = run("sleep 0.05 && printf 'ok\\n'").expect("bash result");

    assert!(output.contains("Exit code: 0"), "{output}");
    assert!(!output.contains("command not found"), "{output}");
    assert!(output.contains("ok"), "{output}");
}

#[test]
fn empty_success_is_only_the_exit_code() {
    if !bash_is_available() {
        return;
    }

    assert_eq!(run("true").expect("bash result"), "Exit code: 0");
}

#[test]
fn a_missing_bash_reports_an_actionable_non_retryable_unavailable_error() {
    let fixture = tempfile::tempdir().expect("fixture");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let missing = fixture.path().join("missing-bash");
    let locator = BashLocator::for_tests(
        Some(missing.into_os_string()),
        Vec::new(),
        std::ffi::OsString::new(),
    );
    let error = execute_output(
        &root,
        &locator,
        None,
        &request("true"),
        Duration::from_secs(20),
        &CancellationToken::new(),
    )
    .expect_err("bash is unavailable in this locator");

    assert!(matches!(error, ProcessError::Unavailable(_)));
    let message = error.to_string();
    assert!(message.contains("CODEXSHIM_BASH"), "{message}");
}

#[test]
fn a_command_reports_merged_output_the_resolved_bash_and_the_exit_code() {
    if !bash_is_available() {
        return;
    }
    let runtime = BashLocator::capture()
        .resolve(&CancellationToken::new())
        .expect("probed bash");

    let output = run("printf 'out\\n'; printf 'err\\n' >&2; exit 3").expect("bash result");

    assert!(output.contains(&format!("Bash: {}", runtime.executable.display())));
    assert!(output.contains("--- output ---"));
    assert!(output.contains("out"));
    assert!(output.contains("err"));
    assert!(output.contains("Exit code: 3"));
    assert!(output.contains("Duration ms:"));
    assert!(!output.contains("Complete."));
    assert!(!output.contains("--- stdout ---"));
}

#[test]
fn the_injected_environment_reaches_the_command() {
    if !bash_is_available() {
        return;
    }
    let output = run("printf '%s %s %s\\n' \"$NO_COLOR\" \"$GIT_EDITOR\" \"$PYTHONUNBUFFERED\"")
        .expect("bash result");

    assert!(output.contains("1 true 1"), "{output}");
    assert!(output.contains("Exit code: 0"));
}

#[cfg(windows)]
#[test]
fn windows_native_arguments_can_keep_slash_switches_literal() {
    if !bash_is_available() {
        return;
    }
    let fixture = tempfile::tempdir().expect("fixture");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let locator = BashLocator::capture();
    let native_command = windows_argument_echo_command(fixture.path());

    let default_command = format!("unset MSYS_NO_PATHCONV MSYS2_ARG_CONV_EXCL; {native_command}");
    let default_output = execute_output(
        &root,
        &locator,
        None,
        &request(&default_command),
        Duration::from_secs(20),
        &CancellationToken::new(),
    )
    .expect("default conversion output")
    .text;
    assert!(default_output.contains("E:/"), "{default_output}");

    let mut disabled = request(&native_command);
    disabled.msys_argument_conversion = MsysArgumentConversion::Disabled;
    let disabled_output = execute_output(
        &root,
        &locator,
        None,
        &disabled,
        Duration::from_secs(20),
        &CancellationToken::new(),
    )
    .expect("disabled conversion output")
    .text;
    assert!(disabled_output.contains("/E"), "{disabled_output}");
    assert!(!disabled_output.contains("E:/"), "{disabled_output}");
}

#[test]
fn a_login_shell_profile_is_never_sourced() {
    if !bash_is_available() {
        return;
    }
    let output = run(
        "printf '%s\\n' \"${BASH_ENV:-unset}\"; shopt -q login_shell && echo login || echo nologin",
    )
    .expect("bash result");

    assert!(output.contains("nologin"), "{output}");
}

#[test]
fn interleaved_writes_keep_pipe_order_in_a_single_output_section() {
    if !bash_is_available() {
        return;
    }
    let output = run(
        "for i in 1 2 3 4 5 6; do if [ $((i % 2)) -eq 0 ]; then printf 'e%s\\n' \"$i\" >&2; else printf 'o%s\\n' \"$i\"; fi; done",
    )
    .expect("bash result");

    let body = output
        .split_once("--- output ---\n")
        .and_then(|(_, rest)| rest.split_once("\nExit code:"))
        .map(|(body, _)| body.to_owned())
        .expect("output section");
    let observed = body
        .lines()
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    assert_eq!(observed, ["o1", "e2", "o3", "e4", "o5", "e6"], "{output}");
}

#[test]
fn a_timeout_terminates_the_tree_and_reports_the_partial_output() {
    if !bash_is_available() {
        return;
    }
    let fixture = tempfile::tempdir().expect("fixture");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let mut timed = request("printf 'before timeout\\n'; sleep 30");
    timed.timeout_ms = Some(2_000);
    let locator = BashLocator::capture();
    locator
        .resolve(&CancellationToken::new())
        .expect("pre-resolved bash");

    let error = execute_output(
        &root,
        &locator,
        None,
        &timed,
        Duration::from_millis(2_000),
        &CancellationToken::new(),
    )
    .expect_err("timeout");

    assert!(matches!(error, ProcessError::Timeout { .. }));
    let report = error.to_string();
    assert!(report.contains("before timeout"), "{report}");
    assert!(report.contains("Exit code: unavailable (timed out)"));
    assert!(report.ends_with("Incomplete."));
}
