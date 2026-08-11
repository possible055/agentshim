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
    CompletedProcess, MAX_STDIN_BYTES, PROCESS_MEMORY_BYTES, ProcessRequest, TimedOutProcess,
    execute, execute_output, render_completed, render_timeout,
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

mod output;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;
