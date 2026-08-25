use std::{
    collections::VecDeque,
    io::Read,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use napi::{
    Env, Error, Result, Unknown,
    bindgen_prelude::{spawn, spawn_blocking},
};
use napi_derive::napi;
use tokio_util::sync::CancellationToken;

use agentshim_core::platform::process::DetachedTree;
use agentshim_core::tools::exec::CaptureSink;

use crate::capture::{ArtifactRecord, CAPTURE_IO_FAILED_CODE, CallCapture, should_publish};
use crate::engine::{EngineState, detached_native_work, native_promise};
use crate::process::{NativeFailure, NativeVoidResult, process_failure};

/// Live preview buffer ceiling: the adapter's `readOutput()` drains a rolling
/// 1 MiB window of recent text; the raw capture artifact is always lossless.
const LIVE_BUFFER_BYTES: usize = 1024 * 1024;

/// Pipe drain chunk size; matches the foreground capture drain.
const DRAIN_CHUNK_BYTES: usize = 8 * 1024;

/// Maximum time to wait for the process tree to fully settle after the primary
/// process exits or after a cancellation request.
const SETTLE_DEADLINE: Duration = Duration::from_secs(5);

/// One published raw capture artifact stream.
#[napi(object)]
#[derive(Clone)]
pub struct ArtifactPublished {
    pub path: String,
    pub bytes: f64,
    pub complete: bool,
    pub stream: String,
}

/// The native job outcome consumed by the adapter's `JobHooks.done`.
#[napi(object)]
#[derive(Clone)]
pub struct NativeJobOutcome {
    pub status: String,
    pub detail: String,
    pub exit_code: Option<String>,
    pub limit_exceeded: bool,
    pub artifacts: Vec<ArtifactPublished>,
    pub failure: Option<NativeFailure>,
}

#[napi(object, object_from_js = false)]
pub struct NativeJobHandleResult {
    pub value: Option<EngineJobHandle>,
    pub failure: Option<NativeFailure>,
}

#[napi(object)]
pub struct NativeStringResult {
    pub value: Option<String>,
    pub failure: Option<NativeFailure>,
}

#[napi(object)]
pub struct NativeJobOutcomeResult {
    pub value: Option<NativeJobOutcome>,
    pub failure: Option<NativeFailure>,
}

struct LiveBuffer {
    bytes: VecDeque<u8>,
    dropped_bytes: u64,
}

impl LiveBuffer {
    fn new() -> Self {
        Self {
            bytes: VecDeque::new(),
            dropped_bytes: 0,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        if chunk.len() >= LIVE_BUFFER_BYTES {
            self.dropped_bytes = self
                .dropped_bytes
                .saturating_add(self.bytes.len() as u64)
                .saturating_add((chunk.len() - LIVE_BUFFER_BYTES) as u64);
            self.bytes.clear();
            self.bytes.extend(&chunk[chunk.len() - LIVE_BUFFER_BYTES..]);
            return;
        }
        let overflow = self
            .bytes
            .len()
            .saturating_add(chunk.len())
            .saturating_sub(LIVE_BUFFER_BYTES);
        if overflow > 0 {
            self.bytes.drain(..overflow);
            self.dropped_bytes = self.dropped_bytes.saturating_add(overflow as u64);
        }
        self.bytes.extend(chunk);
    }

    fn drain(&mut self) -> String {
        let marker = if self.dropped_bytes == 0 {
            String::new()
        } else {
            format!(
                "[agentshim: {} live output bytes were omitted from this view; the raw capture artifact is lossless]\n",
                self.dropped_bytes
            )
        };
        let text = String::from_utf8_lossy(self.bytes.make_contiguous());
        let value = marker + &text;
        self.bytes.clear();
        self.dropped_bytes = 0;
        value
    }
}

pub(crate) struct BackgroundJob {
    capture: Arc<CallCapture>,
    live: Mutex<LiveBuffer>,
    tree: Mutex<TreeState>,
    timeout: Duration,
    deadline: Instant,
    settled: AtomicBool,
    thread_done: AtomicBool,
    finished: CancellationToken,
    artifacts: Arc<Mutex<Vec<ArtifactRecord>>>,
    inline_output_bytes: u64,
    cancel_reason: Mutex<Option<String>>,
    done_tx: tokio::sync::watch::Sender<Option<NativeJobOutcome>>,
    background_permit: Mutex<Option<tokio::sync::OwnedSemaphorePermit>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BackgroundStopCause {
    Explicit,
    Timeout,
    Shutdown,
}

#[derive(Clone)]
struct TreeResult {
    cause: Option<BackgroundStopCause>,
    exit_code: Option<String>,
    verified: bool,
}

enum TreeState {
    Running(DetachedTree),
    Stopping(BackgroundStopCause),
    Settled(TreeResult),
}

impl BackgroundJob {
    fn settle(&self, outcome: NativeJobOutcome) {
        if self.settled.swap(true, Ordering::SeqCst) {
            return;
        }
        self.background_permit
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        self.finished.cancel();
        let _ = self.done_tx.send(Some(outcome));
    }

    fn timeout_ms(&self) -> u64 {
        u64::try_from(self.timeout.as_millis()).unwrap_or(u64::MAX)
    }

    pub(crate) fn is_settled(&self) -> bool {
        self.settled.load(Ordering::SeqCst) && self.thread_done.load(Ordering::SeqCst)
    }

    pub(crate) fn cancel_from_engine(&self) {
        self.stop(BackgroundStopCause::Shutdown);
    }

    fn cancel_explicit(&self, reason: Option<&str>) {
        if let Some(reason) = reason
            && let Ok(mut slot) = self.cancel_reason.lock()
        {
            slot.get_or_insert_with(|| reason.to_owned());
        }
        self.stop(BackgroundStopCause::Explicit);
    }

    fn stop(&self, cause: BackgroundStopCause) {
        let mut tree = {
            let mut state = self
                .tree
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let TreeState::Running(tree) = &mut *state else {
                return;
            };
            if let Ok(observation) = tree.observe()
                && !observation.tree_running
            {
                let exit_code = observation.primary_exit;
                *state = TreeState::Settled(TreeResult {
                    cause: None,
                    exit_code,
                    verified: true,
                });
                return;
            }
            let TreeState::Running(tree) =
                std::mem::replace(&mut *state, TreeState::Stopping(cause))
            else {
                unreachable!("matched running tree")
            };
            tree
        };
        let terminated = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            tree.terminate_and_wait(Instant::now() + SETTLE_DEADLINE)
        }))
        .is_ok_and(|result| result.is_ok());
        let exit_code = tree
            .observe()
            .ok()
            .and_then(|observation| observation.primary_exit);
        let mut state = self
            .tree
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if matches!(*state, TreeState::Stopping(owner) if owner == cause) {
            *state = TreeState::Settled(TreeResult {
                cause: Some(cause),
                exit_code,
                verified: terminated,
            });
        }
    }
}

impl Drop for BackgroundJob {
    fn drop(&mut self) {
        self.stop(BackgroundStopCause::Shutdown);
    }
}

fn artifact_published(record: &ArtifactRecord) -> ArtifactPublished {
    ArtifactPublished {
        path: record.path.to_string_lossy().into_owned(),
        bytes: record.bytes as f64,
        complete: record.complete,
        stream: record.stream.clone(),
    }
}

fn drain_and_settle(job: Arc<BackgroundJob>, mut reader: std::fs::File) {
    let mut buf = vec![0u8; DRAIN_CHUNK_BYTES];
    let mut capture_error: Option<String> = None;
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if let Err(error) = job.capture.append(0, &buf[..n]) {
                    capture_error = Some(error.to_string());
                    job.cancel_explicit(None);
                    break;
                }
                if let Ok(mut live) = job.live.lock() {
                    live.push(&buf[..n]);
                }
            }
            Err(error) => {
                if error.kind() == std::io::ErrorKind::WouldBlock {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                capture_error = Some(error.to_string());
                job.cancel_explicit(None);
                break;
            }
        }
    }

    drop(reader);
    finish_job(&job, capture_error);
}

fn finish_job(job: &BackgroundJob, capture_error: Option<String>) {
    let tree_result = settle_tree(job);
    let limit_exceeded = job.capture.exceeded();
    let complete = !limit_exceeded && tree_result.cause.is_none();
    let records = match job.capture.publish(complete) {
        Ok(records) => records,
        Err(error) => {
            let code = if limit_exceeded {
                crate::capture::CAPTURE_LIMIT_EXCEEDED_CODE
            } else {
                CAPTURE_IO_FAILED_CODE
            };
            let detail = format!("{code}: {error}");
            if let Ok(mut live) = job.live.lock() {
                live.push(format!("\n[agentshim job failed: {detail}]\n").as_bytes());
            }
            job.settle(NativeJobOutcome {
                status: "failed".to_owned(),
                detail,
                exit_code: tree_result.exit_code,
                limit_exceeded,
                artifacts: Vec::new(),
                failure: Some(NativeFailure {
                    code: code.to_owned(),
                    message: error.to_string(),
                    retryable: !limit_exceeded,
                    details: Some(serde_json::json!({
                        "kind": "capture_publish",
                        "limitBytes": job.capture.max_bytes,
                    })),
                }),
            });
            return;
        }
    };
    finish_published_job(
        job,
        capture_error,
        tree_result,
        limit_exceeded,
        complete,
        records,
    );
}

fn settle_tree(job: &BackgroundJob) -> TreeResult {
    let deadline = Instant::now() + SETTLE_DEADLINE;
    loop {
        {
            let mut state = job
                .tree
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match &mut *state {
                TreeState::Running(tree) => {
                    if let Ok(observation) = tree.observe()
                        && !observation.tree_running
                    {
                        let result = TreeResult {
                            cause: None,
                            exit_code: observation.primary_exit,
                            verified: true,
                        };
                        *state = TreeState::Settled(result.clone());
                        return result;
                    }
                }
                TreeState::Stopping(_) => {}
                TreeState::Settled(result) => return result.clone(),
            }
        }
        if Instant::now() >= deadline {
            job.stop(BackgroundStopCause::Shutdown);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn finish_published_job(
    job: &BackgroundJob,
    capture_error: Option<String>,
    tree_result: TreeResult,
    limit_exceeded: bool,
    complete: bool,
    records: Vec<ArtifactRecord>,
) {
    let publish = should_publish(&records, complete, job.inline_output_bytes);
    let artifacts: Vec<ArtifactPublished> = if publish {
        if let Ok(mut table) = job.artifacts.lock() {
            table.extend(records.iter().cloned());
        }
        records.iter().map(artifact_published).collect()
    } else {
        job.capture.discard();
        Vec::new()
    };

    if !artifacts.is_empty()
        && let Ok(mut live) = job.live.lock()
    {
        for artifact in &artifacts {
            live.push(
                format!(
                    "\n[agentshim raw capture: {} ({} bytes, {})]\n",
                    artifact.path,
                    artifact.bytes,
                    if artifact.complete {
                        "complete"
                    } else {
                        "incomplete"
                    }
                )
                .as_bytes(),
            );
        }
    }

    let (status, detail) =
        job_status_detail(job, capture_error.as_deref(), &tree_result, limit_exceeded);

    let failure = if !tree_result.verified {
        Some(NativeFailure {
            code: "AGENTSHIM_OUTCOME_UNCERTAIN".to_owned(),
            message: detail.clone(),
            retryable: true,
            details: Some(serde_json::json!({
                "kind": "teardown",
                "cause": tree_result.cause.map(background_stop_label),
            })),
        })
    } else if limit_exceeded {
        Some(NativeFailure {
            code: crate::capture::CAPTURE_LIMIT_EXCEEDED_CODE.to_owned(),
            message: "capture limit exceeded".to_owned(),
            retryable: false,
            details: Some(serde_json::json!({
                "kind": "capture",
                "limitBytes": job.capture.max_bytes,
            })),
        })
    } else {
        capture_error.map(|message| NativeFailure {
            code: CAPTURE_IO_FAILED_CODE.to_owned(),
            message,
            retryable: true,
            details: Some(serde_json::json!({ "kind": "capture" })),
        })
    };
    let exit_code = tree_result.exit_code;
    job.settle(NativeJobOutcome {
        status: status.to_owned(),
        detail,
        exit_code,
        limit_exceeded,
        artifacts,
        failure,
    });
}

const fn background_stop_label(cause: BackgroundStopCause) -> &'static str {
    match cause {
        BackgroundStopCause::Explicit => "explicit termination",
        BackgroundStopCause::Timeout => "timeout",
        BackgroundStopCause::Shutdown => "shutdown",
    }
}

fn job_status_detail(
    job: &BackgroundJob,
    capture_error: Option<&str>,
    tree_result: &TreeResult,
    limit_exceeded: bool,
) -> (&'static str, String) {
    let exit_code = tree_result.exit_code.as_ref();
    if let Some(error) = capture_error {
        return ("failed", error.to_owned());
    }
    if !tree_result.verified {
        return (
            "failed",
            format!(
                "outcome uncertain after {}",
                tree_result
                    .cause
                    .map_or("natural completion", background_stop_label)
            ),
        );
    }
    if tree_result.cause == Some(BackgroundStopCause::Timeout) {
        return (
            "timed_out",
            format!("background timeout elapsed after {} ms", job.timeout_ms()),
        );
    }
    if tree_result.cause.is_some() {
        let reason = job
            .cancel_reason
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let base = exit_code.map_or("terminated".to_owned(), |code| {
            format!("exit code: {}", code.as_str())
        });
        let detail = match reason.as_deref() {
            Some(reason) => format!("{base} (cancel reason: {reason})"),
            None => base,
        };
        return ("killed", detail);
    }
    if limit_exceeded {
        return ("failed", "capture limit exceeded".to_owned());
    }
    (
        "completed",
        format!(
            "exit code: {}",
            exit_code.map_or("unknown", |code| code.as_str())
        ),
    )
}

/// Opaque handle to one native background bash job. The handle owns the process
/// tree, the durable capture, and the live preview buffer; dropping or disposing
/// it cancels the job and settles the `done` promise.
#[napi]
pub struct EngineJobHandle {
    job: Arc<BackgroundJob>,
    drain_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

#[napi]
impl EngineJobHandle {
    /// Synchronous, idempotent cancellation: routes to the process tree's
    /// terminate and lets the drain thread settle `done`.
    #[napi]
    pub fn cancel(&self, reason: String) -> Result<NativeVoidResult> {
        self.job.cancel_explicit(Some(reason.as_str()));
        let _ = reason;
        Ok(NativeVoidResult {
            value: true,
            failure: None,
        })
    }

    /// Drain the 1 MiB live preview buffer; returns the accumulated text with an
    /// omission marker prefix when bytes were dropped from the view.
    #[napi]
    pub fn read_output(&self) -> Result<NativeStringResult> {
        match self.job.live.lock() {
            Ok(mut live) => Ok(NativeStringResult {
                value: Some(live.drain()),
                failure: None,
            }),
            Err(_) => Ok(NativeStringResult {
                value: None,
                failure: Some(NativeFailure {
                    code: "AGENTSHIM_BACKGROUND_BUFFER_FAILED".to_owned(),
                    message: "background live buffer is unavailable".to_owned(),
                    retryable: true,
                    details: Some(serde_json::json!({ "kind": "live_buffer" })),
                }),
            }),
        }
    }

    /// Resolves when the process tree settles; never rejects. Native failures
    /// arrive as `{ status: 'failed' }`.
    #[napi(ts_return_type = "Promise<NativeJobOutcomeResult>")]
    pub fn done(&self, env: Env) -> Result<Unknown<'static>> {
        let job = Arc::clone(&self.job);
        native_promise(env, detached_native_work(), async move {
            Ok(Self::done_inner(job).await)
        })
    }

    async fn done_inner(job: Arc<BackgroundJob>) -> NativeJobOutcomeResult {
        let mut rx = job.done_tx.subscribe();
        if let Some(outcome) = rx.borrow().clone() {
            return NativeJobOutcomeResult {
                value: Some(outcome),
                failure: None,
            };
        }
        if rx.changed().await.is_err() {
            return NativeJobOutcomeResult {
                value: None,
                failure: Some(NativeFailure {
                    code: "AGENTSHIM_BACKGROUND_DONE_CHANNEL_FAILED".to_owned(),
                    message: "background job completion channel closed".to_owned(),
                    retryable: true,
                    details: Some(serde_json::json!({ "kind": "done_channel" })),
                }),
            };
        }
        match rx.borrow().clone() {
            Some(outcome) => NativeJobOutcomeResult {
                value: Some(outcome),
                failure: None,
            },
            None => NativeJobOutcomeResult {
                value: None,
                failure: Some(NativeFailure {
                    code: "AGENTSHIM_BACKGROUND_OUTCOME_MISSING".to_owned(),
                    message: "background job settled without an outcome".to_owned(),
                    retryable: true,
                    details: Some(serde_json::json!({ "kind": "outcome" })),
                }),
            },
        }
    }

    /// Cancel if not already settled and join the drain thread. Safe to call
    /// multiple times; the handle is inert after disposal.
    #[napi(ts_return_type = "Promise<NativeVoidResult>")]
    pub fn dispose(&self, env: Env) -> Result<Unknown<'static>> {
        if !self.job.settled.load(Ordering::SeqCst) {
            self.job.cancel_from_engine();
        }
        let handle = {
            let mut guard = self
                .drain_thread
                .lock()
                .map_err(|_| Error::new(napi::Status::GenericFailure, "drain handle poisoned"))?;
            guard.take()
        };
        native_promise(env, detached_native_work(), async move {
            if let Some(handle) = handle {
                spawn_blocking(move || {
                    let _ = handle.join();
                })
                .await
                .map_err(|error| Error::new(napi::Status::GenericFailure, error.to_string()))?;
            }
            Ok(NativeVoidResult {
                value: true,
                failure: None,
            })
        })
    }
}

impl Drop for EngineJobHandle {
    fn drop(&mut self) {
        if !self.job.settled.load(Ordering::SeqCst) {
            self.job.cancel_from_engine();
        }
        let guard = self.drain_thread.lock();
        if let Ok(mut guard) = guard {
            if let Some(handle) = guard.take() {
                let _ = handle.join();
            }
        }
    }
}

fn start_drain_thread(
    job: &Arc<BackgroundJob>,
    reader: std::fs::File,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let job = Arc::clone(job);
    std::thread::Builder::new()
        .name("agentshim-background-drain".to_owned())
        .spawn(move || {
            let drained = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                drain_and_settle(Arc::clone(&job), reader);
            }));
            if drained.is_err() && !job.settled.load(Ordering::SeqCst) {
                job.cancel_from_engine();
                job.settle(NativeJobOutcome {
                    status: "failed".to_owned(),
                    detail: "background drain worker panicked".to_owned(),
                    exit_code: None,
                    limit_exceeded: job.capture.exceeded(),
                    artifacts: Vec::new(),
                    failure: Some(NativeFailure {
                        code: "AGENTSHIM_NATIVE_THREAD_FAILED".to_owned(),
                        message: "background drain worker panicked".to_owned(),
                        retryable: true,
                        details: Some(serde_json::json!({
                            "kind": "native_thread",
                            "operation": "background_drain",
                        })),
                    }),
                });
            }
            job.thread_done.store(true, Ordering::SeqCst);
        })
}

fn arm_deadline(job: &Arc<BackgroundJob>, shutdown: CancellationToken) {
    let weak_job = Arc::downgrade(job);
    let finished = job.finished.clone();
    let deadline = job.deadline;
    std::mem::drop(spawn(async move {
        if agentshim_core::runtime::deadline::wait(deadline, finished, shutdown).await
            != agentshim_core::runtime::deadline::DeadlineEvent::Expired
        {
            return;
        }
        if let Some(job) = weak_job.upgrade() {
            std::mem::drop(tokio::task::spawn_blocking(move || {
                job.stop(BackgroundStopCause::Timeout);
            }));
        }
    }));
}

fn try_admit_background(
    state: &EngineState,
) -> std::result::Result<tokio::sync::OwnedSemaphorePermit, NativeFailure> {
    state.resources.try_admit_background().ok_or_else(|| {
        NativeFailure::new(
            "AGENTSHIM_RESOURCE_BUSY",
            "background process capacity is busy",
            true,
            Some(serde_json::json!({ "resource": "background_process" })),
        )
    })
}

pub(crate) fn start_background_prepared(
    state: &EngineState,
    call_id: String,
    handle: String,
    wrapped_argv: Option<&[String]>,
) -> std::result::Result<EngineJobHandle, NativeFailure> {
    if state.shutdown.is_cancelled() {
        return Err(NativeFailure::engine_closed());
    }
    let cancellation = state.call_token(&call_id)?;
    if cancellation.is_cancelled() {
        return Err(NativeFailure::cancelled("background_prepare"));
    }
    let background_permit = try_admit_background(state)?;
    let prepared = state.take_bash(&handle)?;
    let context = agentshim_core::OperationContext::new(
        cancellation.clone(),
        Arc::new(state.output_limits.clone()),
    );

    let call_key = uuid::Uuid::new_v4().simple().to_string();
    let capture = CallCapture::create(
        &state.capture_root,
        &state.session_key,
        &call_key,
        &["output"],
        state.capture_max_bytes,
    )
    .map_err(|error| {
        NativeFailure::new(
            CAPTURE_IO_FAILED_CODE,
            error.to_string(),
            true,
            Some(serde_json::json!({
                "kind": "capture_create",
                "limitBytes": state.capture_max_bytes,
            })),
        )
    })?;
    let capture = Arc::new(capture);

    let (tree, reader, effective_timeout, spawned_at) = match state
        .tool_engine
        .spawn_background_bash(prepared, wrapped_argv, &context)
    {
        Ok(spawned) => spawned,
        Err(error) => {
            capture.discard();
            return Err(process_failure(&error));
        }
    };
    if cancellation.is_cancelled() {
        let mut tree = tree;
        let _ = tree.terminate_and_wait(Instant::now() + SETTLE_DEADLINE);
        capture.discard();
        return Err(NativeFailure::cancelled("background_spawn"));
    }

    let (done_tx, _done_rx) = tokio::sync::watch::channel(None);
    let deadline = spawned_at + effective_timeout;
    let job = Arc::new(BackgroundJob {
        capture: Arc::clone(&capture),
        live: Mutex::new(LiveBuffer::new()),
        tree: Mutex::new(TreeState::Running(tree)),
        timeout: effective_timeout,
        deadline,
        settled: AtomicBool::new(false),
        thread_done: AtomicBool::new(false),
        finished: CancellationToken::new(),
        artifacts: Arc::clone(&state.artifacts),
        inline_output_bytes: state.output_limits.capture_publish_bytes(),
        cancel_reason: Mutex::new(None),
        done_tx,
        background_permit: Mutex::new(Some(background_permit)),
    });
    {
        let mut backgrounds = state
            .backgrounds
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.shutdown.is_cancelled() || cancellation.is_cancelled() {
            drop(backgrounds);
            job.cancel_from_engine();
            capture.discard();
            return Err(NativeFailure::cancelled("background_commit"));
        }
        backgrounds.retain(|existing| existing.strong_count() > 0);
        backgrounds.push(Arc::downgrade(&job));
    }

    let drain_thread = start_drain_thread(&job, reader).map_err(|error| {
        job.cancel_from_engine();
        capture.discard();
        NativeFailure::new(
            "AGENTSHIM_NATIVE_THREAD_FAILED",
            error.to_string(),
            true,
            Some(serde_json::json!({
                "kind": "native_thread",
                "operation": "background_drain_spawn",
            })),
        )
    })?;

    arm_deadline(&job, state.shutdown.child_token());

    Ok(EngineJobHandle {
        job,
        drain_thread: Mutex::new(Some(drain_thread)),
    })
}

#[cfg(test)]
mod tests {
    use super::{LIVE_BUFFER_BYTES, LiveBuffer};

    #[test]
    fn live_buffer_keeps_latest_utf8_bytes_with_exact_omission_count() {
        let mut live = LiveBuffer::new();
        live.push(&vec![b'x'; LIVE_BUFFER_BYTES - 1]);
        live.push("🦀".as_bytes());

        let output = live.drain();
        assert!(output.starts_with(
            "[agentshim: 3 live output bytes were omitted from this view; the raw capture artifact is lossless]\n"
        ));
        assert!(output.ends_with('🦀'));
    }
}
