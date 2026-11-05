use std::{sync::Arc, time::Duration};

use tokio_util::sync::CancellationToken;

use crate::{
    path::RepositoryRoot,
    tools::exec::{
        ProcessError,
        capture::{Capture, capture_bytes_per_stream},
        spawn::default_max_timeout_ms,
    },
};

use super::{
    BASH_ENVIRONMENT, BashRequest, CompletedBash, MsysArgumentConversion, STRIPPED_INHERITED_ENV,
    bash_environment,
    detached::DetachedTrees,
    execute_output, execute_output_with_budget,
    locate::{self, BashLocator},
    render_completed,
};

#[test]
fn token_dense_bash_output_keeps_head_tail_and_metadata() {
    let mut capture = Capture::new(capture_bytes_per_stream(1, crate::output::MODEL_BYTE_LIMIT));
    capture.push(format!("HEAD\n{}\nTAIL\n", " x".repeat(20_000)).as_bytes());
    let cancellation = CancellationToken::new();
    let output = render_completed(
        &CompletedBash {
            bash: "/usr/bin/bash".into(),
            cwd: "workspace".into(),
            exit: "7".to_owned(),
            duration: Duration::from_millis(2),
            output: capture,
            msys_retry_available: false,
        },
        &cancellation,
    )
    .expect("bounded bash output");

    assert!(output.fits_budget(&crate::output::TestCallBudget::default()));
    assert!(output.contains("HEAD"));
    assert!(output.contains("TAIL"));
    assert!(output.contains("bytes omitted"));
    assert!(output.contains("Exit code: 7"));
    assert!(output.contains("Output: total="));
    assert!(!output.contains("Complete."));
}

fn request(command: &str) -> BashRequest {
    BashRequest {
        command: command.to_owned(),
        cwd: None,
        timeout_ms: Some(20_000),
        detach: false,
        log_path: None,
        msys_argument_conversion: MsysArgumentConversion::Default,
    }
}

fn trees() -> DetachedTrees {
    DetachedTrees::new(16)
}

fn run(command: &str) -> Result<String, ProcessError> {
    let fixture = tempfile::tempdir().expect("fixture");
    let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
    let locator = BashLocator::capture();
    execute_output(
        &root,
        &locator,
        None,
        &request(command),
        Duration::from_secs(20),
        &CancellationToken::new(),
    )
    .map(|output| output.text)
}

#[test]
fn slash_switch_detection_separates_windows_switches_from_posix_paths() {
    for switch in ["/E", "/S", "/MIR", "/XO", "/T4"] {
        assert!(super::is_slash_switch(switch), "{switch} is a switch");
    }
    for path in [
        "/tmp",
        "/usr",
        "/etc",
        "/home",
        "/usr/bin",
        "/",
        "a/b",
        "https://example.test",
        "--flag",
    ] {
        assert!(!super::is_slash_switch(path), "{path} is not a switch");
    }
}

/// ash passes `/E` through literally, so the Git-Bash-specific retry hint would be noise.
#[test]
fn msys_retry_hint_is_suppressed_for_the_ash_flavor() {
    let request = request("robocopy /E src dst");
    assert!(!super::msys_retry_available(
        &request,
        locate::ShellFlavor::Ash
    ));
}

#[cfg(windows)]
#[test]
fn msys_retry_hint_survives_for_a_bash_runtime() {
    let request = request("robocopy /E src dst");
    assert!(super::msys_retry_available(
        &request,
        locate::ShellFlavor::Bash
    ));
}

fn bash_is_available() -> bool {
    BashLocator::capture()
        .resolve(&CancellationToken::new())
        .is_ok()
}

pub(super) fn read_shell_child_pid(path: &std::path::Path) -> u32 {
    agentshim_test_support::poll::poll_until_within(
        Duration::from_secs(5),
        "shell child PID",
        || match std::fs::read_to_string(path) {
            Ok(text) if text.ends_with('\n') => Some(text.trim().parse().expect("shell child PID")),
            Ok(_) => None,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => panic!("cannot read shell child PID: {error}"),
        },
    )
}

pub(super) fn assert_shell_child_exited(runtime: &locate::BashRuntime, pid: u32) {
    // MSYS shell PIDs are not Win32 PIDs; query them through the same shell runtime.
    let command = format!("if kill -0 {pid} 2>/dev/null; then printf alive; fi");
    agentshim_test_support::poll::poll_until_within(
        Duration::from_secs(5),
        "shell child to exit",
        || {
            let output = std::process::Command::new(&runtime.executable)
                .args(runtime.launch_args(&command))
                .env_remove("BASH_ENV")
                .env_remove("ENV")
                .output()
                .expect("query shell child liveness");
            assert!(output.status.success(), "{output:?}");
            match output.stdout.as_slice() {
                b"alive" => None,
                b"" => Some(()),
                other => panic!("unexpected shell liveness output: {other:?}"),
            }
        },
    );
}

#[cfg(windows)]
fn windows_argument_echo_command(root: &std::path::Path) -> String {
    let script = root.join("echo-argument.ps1");
    std::fs::write(&script, "[Console]::Write($args[0])").expect("PowerShell fixture");
    let script = script
        .to_string_lossy()
        .replace('\\', "/")
        .replace('\'', "'\"'\"'");
    format!("powershell.exe -NoProfile -ExecutionPolicy Bypass -File '{script}' /E")
}

mod busybox;
mod detached;
mod detached_admission;
mod execution;
