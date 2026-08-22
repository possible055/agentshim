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

fn bash_is_available() -> bool {
    BashLocator::capture()
        .resolve(&CancellationToken::new())
        .is_ok()
}

#[cfg(windows)]
fn windows_argument_echo_command(root: &std::path::Path) -> String {
    let script = root.join("echo-argument.ps1");
    std::fs::write(&script, "[Console]::Write($args[0])").expect("PowerShell fixture");
    let script = script
        .to_string_lossy()
        .replace('\\', "/")
        .replace('\'', "'\"'\"'");
    format!("powershell.exe -NoProfile -File '{script}' /E")
}

mod detached;
mod detached_admission;
mod execution;
