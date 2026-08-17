use std::{fmt::Write as _, fs::File, io, path::PathBuf, time::Duration};

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::tools::{
    ToolOutput,
    exec::{
        ProcessError,
        capture::{diagnostic_path, escape_invalid_utf8},
    },
};

pub const DEFAULT_TAIL_BYTES: usize = 8 * 1024;
pub const MAX_TAIL_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BashStatusRequest {
    pub job_id: String,
    #[serde(default = "default_tail_bytes")]
    pub tail_bytes: usize,
    pub cursor: Option<u64>,
    pub max_bytes: Option<usize>,
    #[serde(default)]
    pub wait_ms: u64,
}

const fn default_tail_bytes() -> usize {
    DEFAULT_TAIL_BYTES
}

impl BashStatusRequest {
    pub fn validate(&self) -> Result<(), ProcessError> {
        validate_job_id(&self.job_id)?;
        if self.tail_bytes > MAX_TAIL_BYTES {
            return Err(ProcessError::Validation(format!(
                "tail_bytes must be from 0 to {MAX_TAIL_BYTES}"
            )));
        }
        if self.max_bytes.is_some_and(|value| value > MAX_TAIL_BYTES) {
            return Err(ProcessError::Validation(format!(
                "max_bytes must be from 0 to {MAX_TAIL_BYTES}"
            )));
        }
        if self.wait_ms > 1_000 {
            return Err(ProcessError::Validation(
                "wait_ms must be from 0 to 1000".to_owned(),
            ));
        }
        Ok(())
    }

    pub fn memory_charge(&self) -> usize {
        MAX_TAIL_BYTES
            .saturating_mul(2)
            .saturating_add(self.job_id.len())
    }
}

pub fn validate_job_id(job_id: &str) -> Result<(), ProcessError> {
    let Some(uuid) = job_id.strip_prefix("bash-") else {
        return Err(invalid_job_id());
    };
    match uuid::Uuid::parse_str(uuid) {
        Ok(uuid) if uuid.get_version_num() == 4 => Ok(()),
        _ => Err(invalid_job_id()),
    }
}

fn invalid_job_id() -> ProcessError {
    ProcessError::Validation("job_id must have the form bash-<uuid-v4>".to_owned())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JobState {
    Running,
    StatusUnknown,
    Finalizing,
    Terminating,
    Completed,
    Terminated,
    OutcomeUncertain,
}

impl JobState {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::StatusUnknown => "status_unknown",
            Self::Finalizing => "finalizing",
            Self::Terminating => "terminating",
            Self::Completed => "completed",
            Self::Terminated => "terminated",
            Self::OutcomeUncertain => "outcome_uncertain",
        }
    }
}

#[derive(Clone, Debug)]
pub struct RawLogSnapshot {
    pub total: u64,
    pub start: u64,
    pub bytes: Vec<u8>,
    pub error: Option<String>,
}

impl RawLogSnapshot {
    pub fn empty_with_error(error: &str) -> Self {
        Self {
            total: 0,
            start: 0,
            bytes: Vec::new(),
            error: Some(bounded_diagnostic(error)),
        }
    }
}

#[derive(Clone, Debug)]
pub struct JobSnapshot {
    pub job_id: String,
    pub state: JobState,
    pub pid: u32,
    pub runtime: Duration,
    pub primary_exit: Option<String>,
    pub log_path: PathBuf,
    pub log: RawLogSnapshot,
    pub outcome: Option<&'static str>,
}

pub fn snapshot_tail(file: &File, requested: usize) -> RawLogSnapshot {
    match snapshot_tail_once(file, requested) {
        Ok(snapshot) => snapshot,
        Err(first) => snapshot_tail_once(file, requested).unwrap_or_else(|second| {
            RawLogSnapshot::empty_with_error(&format!("{first}; retry: {second}"))
        }),
    }
}

fn snapshot_tail_once(file: &File, requested: usize) -> io::Result<RawLogSnapshot> {
    let total = file.metadata()?.len();
    let shown = usize::try_from(total.min(u64::try_from(requested).unwrap_or(u64::MAX)))
        .unwrap_or(requested);
    let start = total.saturating_sub(u64::try_from(shown).unwrap_or(u64::MAX));
    let mut bytes = vec![0_u8; shown];
    let mut offset = 0_usize;
    while offset < bytes.len() {
        let position = start.saturating_add(u64::try_from(offset).unwrap_or(u64::MAX));
        let read = positioned_read(file, &mut bytes[offset..], position)?;
        if read == 0 {
            bytes.truncate(offset);
            break;
        }
        offset = offset.saturating_add(read);
    }
    Ok(RawLogSnapshot {
        total,
        start,
        bytes,
        error: None,
    })
}

pub fn snapshot_from(
    file: &File,
    cursor: u64,
    requested: usize,
    finalized: bool,
) -> RawLogSnapshot {
    match snapshot_from_once(file, cursor, requested, finalized) {
        Ok(snapshot) => snapshot,
        Err(error) => RawLogSnapshot::empty_with_error(&error.to_string()),
    }
}

fn snapshot_from_once(
    file: &File,
    cursor: u64,
    requested: usize,
    finalized: bool,
) -> io::Result<RawLogSnapshot> {
    let total = file.metadata()?.len();
    let start = cursor.min(total);
    let available = total.saturating_sub(start);
    let shown = usize::try_from(available.min(u64::try_from(requested).unwrap_or(u64::MAX)))
        .unwrap_or(requested);
    let mut bytes = vec![0_u8; shown];
    let mut offset = 0_usize;
    while offset < bytes.len() {
        let position = start.saturating_add(u64::try_from(offset).unwrap_or(u64::MAX));
        let read = positioned_read(file, &mut bytes[offset..], position)?;
        if read == 0 {
            bytes.truncate(offset);
            break;
        }
        offset = offset.saturating_add(read);
    }
    let reaches_end = start.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX)) >= total;
    if let Err(error) = std::str::from_utf8(&bytes)
        && error.error_len().is_none()
        && !(finalized && reaches_end)
    {
        bytes.truncate(error.valid_up_to());
    }
    Ok(RawLogSnapshot {
        total,
        start,
        bytes,
        error: None,
    })
}

#[cfg(unix)]
fn positioned_read(file: &File, buffer: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buffer, offset)
}

#[cfg(windows)]
fn positioned_read(file: &File, buffer: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buffer, offset)
}

pub fn render_with_budget(
    snapshot: &JobSnapshot,
    requested_tail_bytes: usize,
    cancellation: &CancellationToken,
    output_budget: &dyn crate::output::CallBudget,
) -> Result<ToolOutput, ProcessError> {
    let available = requested_tail_bytes.min(snapshot.log.bytes.len());
    let full = render(snapshot, available);
    if full.fits_content_and_call(output_budget, cancellation) {
        return Ok(full);
    }
    if cancellation.is_cancelled() {
        return Err(crate::output::OutputError::Cancelled.into());
    }
    let minimal = render(snapshot, 0);
    if !minimal.fits_content_and_call(output_budget, cancellation) {
        return Err(crate::output::OutputError::RequiredContentTooLarge.into());
    }
    let mut low = 0_usize;
    let mut high = available.saturating_add(1);
    while low + 1 < high {
        if cancellation.is_cancelled() {
            return Err(crate::output::OutputError::Cancelled.into());
        }
        let midpoint = low + (high - low) / 2;
        let candidate = render(snapshot, midpoint);
        if candidate.fits_content_and_call(output_budget, cancellation) {
            low = midpoint;
        } else {
            high = midpoint;
        }
    }
    Ok(render(snapshot, low))
}

pub fn render_termination_with_budget(
    snapshot: &JobSnapshot,
    cancellation: &CancellationToken,
    output_budget: &dyn crate::output::CallBudget,
) -> Result<ToolOutput, ProcessError> {
    let outcome = snapshot.outcome.unwrap_or(match snapshot.state {
        JobState::Terminated => "verified",
        JobState::OutcomeUncertain => "outcome_uncertain",
        _ => "already_terminal",
    });
    let output = ToolOutput::new(format!(
        "Job: {}\nState: {}\nPID: {}\nOutcome: {}\nRuntime ms: {}\nLog: {}",
        snapshot.job_id,
        snapshot.state.label(),
        snapshot.pid,
        outcome,
        u64::try_from(snapshot.runtime.as_millis()).unwrap_or(u64::MAX),
        diagnostic_path(&snapshot.log_path),
    ));
    if !output.fits_content_and_call(output_budget, cancellation) {
        return Err(crate::output::OutputError::RequiredContentTooLarge.into());
    }
    Ok(output)
}

fn render(snapshot: &JobSnapshot, tail_bytes: usize) -> ToolOutput {
    let exit = snapshot.primary_exit.as_deref().unwrap_or("pending");
    let mut text = format!(
        "Job: {}\nState: {}\nPID: {}\nRuntime ms: {}\nExit: {}\nLog: {}",
        snapshot.job_id,
        snapshot.state.label(),
        snapshot.pid,
        u64::try_from(snapshot.runtime.as_millis()).unwrap_or(u64::MAX),
        exit,
        diagnostic_path(&snapshot.log_path),
    );
    if let Some(outcome) = snapshot.outcome {
        text.push_str("\nOutcome: ");
        text.push_str(outcome);
    }
    if let Some(error) = &snapshot.log.error {
        text.push_str("\nLog error: ");
        text.push_str(error);
    } else {
        let shown = tail_bytes.min(snapshot.log.bytes.len());
        let omitted = snapshot
            .log
            .total
            .saturating_sub(u64::try_from(shown).unwrap_or(u64::MAX));
        write!(
            text,
            "\nLog: total={} tail_start={} shown={} omitted={}.",
            snapshot.log.total, omitted, shown, omitted
        )
        .expect("write to string");
        if shown > 0 {
            let bytes = &snapshot.log.bytes[snapshot.log.bytes.len() - shown..];
            let (body, invalid) = escape_invalid_utf8(bytes);
            if invalid > 0 {
                write!(
                    text,
                    "\nEncoding: utf-8-with-byte-escapes invalid_utf8_bytes={invalid}."
                )
                .expect("write to string");
            }
            text.push_str("\n\n--- log tail ---\n");
            text.push_str(&body);
        }
    }
    let (chunk, invalid) = escape_invalid_utf8(&snapshot.log.bytes);
    let next_cursor = snapshot
        .log
        .start
        .saturating_add(u64::try_from(snapshot.log.bytes.len()).unwrap_or(u64::MAX));
    ToolOutput::new(text.clone()).with_structured(serde_json::json!({
        "tool": "bash_status",
        "job": {
            "jobId": snapshot.job_id,
            "state": snapshot.state.label(),
            "exitCode": snapshot.primary_exit,
            "totalBytes": snapshot.log.total,
            "chunkStart": snapshot.log.start,
            "nextCursor": next_cursor,
            "chunk": chunk,
            "invalidUtf8Bytes": invalid,
            "capture": "repository-log",
            "truncated": false,
            "error": snapshot.log.error,
        }
    }))
}

fn bounded_diagnostic(value: &str) -> String {
    const LIMIT: usize = 512;
    if value.len() <= LIMIT {
        return value.to_owned();
    }
    let mut end = LIMIT;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...[truncated]", &value[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_request_bounds_tail_and_requires_v4_job_id() {
        let valid = BashStatusRequest {
            job_id: format!("bash-{}", uuid::Uuid::new_v4()),
            tail_bytes: MAX_TAIL_BYTES,
            cursor: None,
            max_bytes: None,
            wait_ms: 0,
        };
        assert!(valid.validate().is_ok());
        assert!(
            BashStatusRequest {
                tail_bytes: MAX_TAIL_BYTES + 1,
                ..valid.clone()
            }
            .validate()
            .is_err()
        );
        assert!(
            BashStatusRequest {
                job_id: "bash-not-a-uuid".to_owned(),
                ..valid
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn snapshot_is_positioned_and_bounded() {
        let fixture = tempfile::NamedTempFile::new().expect("fixture");
        std::fs::write(fixture.path(), b"0123456789").expect("write");
        let file = std::fs::File::open(fixture.path()).expect("open");
        let snapshot = snapshot_tail(&file, 4);
        assert_eq!(snapshot.total, 10);
        assert_eq!(snapshot.bytes, b"6789");
    }

    #[test]
    fn live_cursor_does_not_split_valid_utf8_and_terminal_incomplete_bytes_remain_visible() {
        let fixture = tempfile::NamedTempFile::new().expect("fixture");
        std::fs::write(fixture.path(), "abcéx").expect("write");
        let file = std::fs::File::open(fixture.path()).expect("open");

        let first = snapshot_from(&file, 0, 4, false);
        assert_eq!(first.start, 0);
        assert_eq!(first.bytes, b"abc");
        let second = snapshot_from(&file, 3, 4, false);
        assert_eq!(second.start, 3);
        assert_eq!(second.bytes, "éx".as_bytes());

        std::fs::write(fixture.path(), [b'a', 0xc3]).expect("incomplete write");
        let live = snapshot_from(&file, 0, 2, false);
        assert_eq!(live.bytes, b"a");
        let terminal = snapshot_from(&file, 0, 2, true);
        assert_eq!(terminal.bytes, [b'a', 0xc3]);
    }
}
