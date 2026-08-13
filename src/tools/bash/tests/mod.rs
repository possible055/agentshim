use std::{sync::Arc, time::Duration};

use tokio_util::sync::CancellationToken;

use crate::{
    path::RepositoryRoot,
    tools::exec::{
        ProcessError,
        capture::{Capture, capture_bytes_per_stream},
        spawn::MAX_TIMEOUT_MS,
    },
};

use super::{
    BASH_ENVIRONMENT, BASH_MEMORY_BYTES, BashRequest, CompletedBash, MsysArgumentConversion,
    detached::DetachedTrees,
    environment, execute_output,
    locate::{self, BashLocator},
    render_completed,
};

#[test]
fn token_dense_bash_output_keeps_head_tail_and_metadata() {
    let mut capture = Capture::new(capture_bytes_per_stream(1));
    capture.push(format!("HEAD\n{}\nTAIL\n", " x".repeat(20_000)).as_bytes());
    let cancellation = CancellationToken::new();
    let output = render_completed(
        &CompletedBash {
            bash: "/usr/bin/bash".into(),
            cwd: "workspace".into(),
            exit: "7".to_owned(),
            duration: Duration::from_millis(2),
            output: capture,
        },
        &cancellation,
    )
    .expect("bounded bash output");

    assert!(output.fits_budget());
    assert!(output.fits_model_budget(&cancellation));
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
mod execution;
