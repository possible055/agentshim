use std::{
    io::Read,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use napi::{Error, Result, bindgen_prelude::spawn_blocking};
use napi_derive::napi;
use tokio_util::sync::CancellationToken;

use agentshim_core::output::CallBudget;
use agentshim_core::platform::process::{DetachedTree, spawn_detached_capture};
use agentshim_core::tools::exec::spawn::{CaptureSink, ExecPlan, Streams};

use crate::capture::{ArtifactRecord, CAPTURE_IO_FAILED_CODE, CallCapture, should_publish};
use crate::engine::EngineState;
use crate::process::NativeFailure;

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

struct LiveBuffer {
    text: String,
    dropped_bytes: u64,
}

impl LiveBuffer {
    fn new() -> Self {
        Self {
            text: String::new(),
            dropped_bytes: 0,
        }
    }

    fn push(&mut self, chunk: &str) {
        self.text.push_str(chunk);
        let bytes = self.text.len();
        if bytes <= LIVE_BUFFER_BYTES {
            return;
        }
        let overflow = bytes - LIVE_BUFFER_BYTES;
        self.dropped_bytes += overflow as u64;
        let cut = self.text.char_indices().nth(overflow).map_or(0, |(i, _)| i);
        self.text = self.text[cut..].to_owned();
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
        let value = marker + &self.text;
        self.text.clear();
        self.dropped_bytes = 0;
        value
    }
}

pub(crate) struct BackgroundJob {
    capture: Arc<CallCapture>,
    live: Mutex<LiveBuffer>,
    tree: Mutex<Option<DetachedTree>>,
    cancelled: AtomicBool,
    settled: AtomicBool,
    thread_done: AtomicBool,
    shutdown: CancellationToken,
    artifacts: Arc<Mutex<Vec<ArtifactRecord>>>,
    done_tx: tokio::sync::watch::Sender<Option<NativeJobOutcome>>,
}

impl BackgroundJob {
    fn settle(&self, outcome: NativeJobOutcome) {
        if self.settled.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = self.done_tx.send(Some(outcome));
    }

    pub(crate) fn is_settled(&self) -> bool {
        self.settled.load(Ordering::SeqCst) && self.thread_done.load(Ordering::SeqCst)
    }

    pub(crate) fn cancel_from_engine(&self) {
        if self.cancelled.swap(true, Ordering::SeqCst) {
            return;
        }
        self.shutdown.cancel();
        if let Ok(mut tree) = self.tree.lock()
            && let Some(tree) = tree.as_mut()
        {
            let _ = tree.terminate_and_wait(Instant::now() + SETTLE_DEADLINE);
        }
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
        if job.cancelled.load(Ordering::SeqCst) {
            break;
        }
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if let Err(error) = job.capture.append(0, &buf[..n]) {
                    capture_error = Some(error.to_string());
                    job.cancel_from_engine();
                    break;
                }
                let text = String::from_utf8_lossy(&buf[..n]);
                if let Ok(mut live) = job.live.lock() {
                    live.push(&text);
                }
            }
            Err(error) => {
                if error.kind() == std::io::ErrorKind::WouldBlock {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                capture_error = Some(error.to_string());
                job.cancel_from_engine();
                break;
            }
        }
    }

    drop(reader);
    finish_job(&job, capture_error);
}

fn finish_job(job: &BackgroundJob, capture_error: Option<String>) {
    let exit_code = settle_tree(job);
    let limit_exceeded = job.capture.exceeded();
    let cancelled = job.cancelled.load(Ordering::SeqCst);
    let complete = !limit_exceeded && !cancelled;
    let records = match job.capture.publish(complete) {
        Ok(records) => records,
        Err(error) => {
            let detail = format!("{CAPTURE_IO_FAILED_CODE}: {error}");
            if let Ok(mut live) = job.live.lock() {
                live.push(&format!("\n[agentshim job failed: {detail}]\n"));
            }
            job.settle(NativeJobOutcome {
                status: "failed".to_owned(),
                detail,
                exit_code,
                limit_exceeded,
                artifacts: Vec::new(),
                failure: Some(NativeFailure {
                    code: CAPTURE_IO_FAILED_CODE.to_owned(),
                    message: error.to_string(),
                    retryable: true,
                    details: None,
                }),
            });
            return;
        }
    };
    finish_published_job(
        job,
        capture_error,
        exit_code,
        limit_exceeded,
        complete,
        records,
    );
}

fn settle_tree(job: &BackgroundJob) -> Option<String> {
    if job.cancelled.load(Ordering::SeqCst) {
        if let Ok(mut guard) = job.tree.lock() {
            if let Some(tree) = guard.as_mut() {
                let deadline = Instant::now() + SETTLE_DEADLINE;
                let _ = tree.terminate_and_wait(deadline);
            }
        }
    }

    let mut exit_code = None;
    let tree = job.tree.lock().ok().and_then(|mut guard| guard.take());
    if let Some(mut tree) = tree {
        let deadline = Instant::now() + SETTLE_DEADLINE;
        while Instant::now() < deadline {
            match tree.observe() {
                Ok(obs) => {
                    if !obs.tree_running {
                        exit_code = obs.primary_exit;
                        break;
                    }
                }
                Err(_) => break,
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    exit_code
}

fn finish_published_job(
    job: &BackgroundJob,
    capture_error: Option<String>,
    exit_code: Option<String>,
    limit_exceeded: bool,
    complete: bool,
    records: Vec<ArtifactRecord>,
) {
    let cancelled = job.cancelled.load(Ordering::SeqCst);
    let publish = should_publish(&records, complete);
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
            live.push(&format!(
                "\n[agentshim raw capture: {} ({} bytes, {})]\n",
                artifact.path,
                artifact.bytes,
                if artifact.complete {
                    "complete"
                } else {
                    "incomplete"
                }
            ));
        }
    }

    let (status, detail) = if let Some(error) = capture_error.as_ref() {
        ("failed", error.clone())
    } else if cancelled {
        (
            "killed",
            exit_code
                .as_ref()
                .map_or("terminated".to_owned(), |code| format!("exit code: {code}")),
        )
    } else if limit_exceeded {
        ("failed", "capture limit exceeded".to_owned())
    } else {
        (
            "completed",
            format!("exit code: {}", exit_code.as_deref().unwrap_or("unknown")),
        )
    };

    job.settle(NativeJobOutcome {
        status: status.to_owned(),
        detail,
        exit_code,
        limit_exceeded,
        artifacts,
        failure: capture_error.map_or_else(
            || {
                limit_exceeded.then(|| NativeFailure {
                    code: crate::capture::CAPTURE_LIMIT_EXCEEDED_CODE.to_owned(),
                    message: "capture limit exceeded".to_owned(),
                    retryable: false,
                    details: None,
                })
            },
            |message| {
                Some(NativeFailure {
                    code: CAPTURE_IO_FAILED_CODE.to_owned(),
                    message,
                    retryable: true,
                    details: None,
                })
            },
        ),
    });
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
    pub fn cancel(&self, reason: String) -> Result<()> {
        self.job.cancel_from_engine();
        let _ = reason;
        Ok(())
    }

    /// Drain the 1 MiB live preview buffer; returns the accumulated text with an
    /// omission marker prefix when bytes were dropped from the view.
    #[napi]
    pub fn read_output(&self) -> Result<String> {
        let mut live = self
            .job
            .live
            .lock()
            .map_err(|_| Error::new(napi::Status::GenericFailure, "live buffer poisoned"))?;
        Ok(live.drain())
    }

    /// Resolves when the process tree settles; never rejects. Native failures
    /// arrive as `{ status: 'failed' }`.
    #[napi(ts_return_type = "Promise<NativeJobOutcome>")]
    pub async fn done(&self) -> Result<NativeJobOutcome> {
        let mut rx = self.job.done_tx.subscribe();
        if let Some(outcome) = rx.borrow().clone() {
            return Ok(outcome);
        }
        rx.changed()
            .await
            .map_err(|_| Error::new(napi::Status::GenericFailure, "done channel closed"))?;
        rx.borrow()
            .clone()
            .ok_or_else(|| Error::new(napi::Status::GenericFailure, "done settled without outcome"))
    }

    /// Cancel if not already settled and join the drain thread. Safe to call
    /// multiple times; the handle is inert after disposal.
    #[napi]
    pub async fn dispose(&self) -> Result<()> {
        if !self.job.settled.load(Ordering::SeqCst) {
            self.cancel("handle disposed".to_owned())?;
        }
        let handle = {
            let mut guard = self
                .drain_thread
                .lock()
                .map_err(|_| Error::new(napi::Status::GenericFailure, "drain handle poisoned"))?;
            guard.take()
        };
        if let Some(handle) = handle {
            spawn_blocking(move || {
                let _ = handle.join();
            })
            .await
            .map_err(|error| Error::new(napi::Status::GenericFailure, error.to_string()))?;
        }
        Ok(())
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

pub(crate) fn start_background_prepared(
    state: &EngineState,
    handle: String,
    wrapped_argv: Option<&[String]>,
) -> Result<EngineJobHandle> {
    if state.shutdown.is_cancelled() {
        return Err(Error::new(napi::Status::GenericFailure, "engine is closed"));
    }
    let prepared = state.take_bash(&handle)?;
    let PreparedBackgroundBash {
        resolved,
        cwd,
        args,
        environment,
    } = build_background_plan(prepared, wrapped_argv, state)?;

    let plan = ExecPlan {
        resolved: &resolved,
        cwd: &cwd,
        args: &args,
        environment: &environment,
        stdin: None,
        streams: Streams::Merged,
        timeout: Duration::from_secs(0),
        capture_page_bytes: state.budget.page_bytes(),
    };

    let call_key = uuid::Uuid::new_v4().simple().to_string();
    let capture = CallCapture::create(
        &state.capture_root,
        &state.session_key,
        &call_key,
        &["output"],
        state.capture_max_bytes,
    )
    .map_err(|error| {
        Error::new(
            napi::Status::GenericFailure,
            format!("{CAPTURE_IO_FAILED_CODE}: {error}"),
        )
    })?;
    let capture = Arc::new(capture);

    let (tree, reader) = match spawn_detached_capture(&plan, &environment) {
        Ok(spawned) => spawned,
        Err(error) => {
            capture.discard();
            return Err(Error::new(napi::Status::GenericFailure, error.to_string()));
        }
    };

    let (done_tx, _done_rx) = tokio::sync::watch::channel(None);
    let job = Arc::new(BackgroundJob {
        capture: Arc::clone(&capture),
        live: Mutex::new(LiveBuffer::new()),
        tree: Mutex::new(Some(tree)),
        cancelled: AtomicBool::new(false),
        settled: AtomicBool::new(false),
        thread_done: AtomicBool::new(false),
        shutdown: state.shutdown.child_token(),
        artifacts: Arc::clone(&state.artifacts),
        done_tx,
    });
    {
        let mut backgrounds = state
            .backgrounds
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        backgrounds.retain(|existing| existing.strong_count() > 0);
        backgrounds.push(Arc::downgrade(&job));
    }

    let job_clone = Arc::clone(&job);
    let drain_thread = std::thread::spawn(move || {
        drain_and_settle(Arc::clone(&job_clone), reader);
        job_clone.thread_done.store(true, Ordering::SeqCst);
    });

    Ok(EngineJobHandle {
        job,
        drain_thread: Mutex::new(Some(drain_thread)),
    })
}

struct PreparedBackgroundBash {
    resolved: agentshim_core::tools::exec::resolve::ResolvedProgram,
    cwd: std::path::PathBuf,
    args: Vec<String>,
    environment: agentshim_core::tools::exec::spawn::EnvironmentPlan,
}

fn build_background_plan(
    prepared: agentshim_core::tools::bash::PreparedBash,
    wrapped_argv: Option<&[String]>,
    state: &EngineState,
) -> Result<PreparedBackgroundBash> {
    let agentshim_core::tools::bash::PreparedBash {
        mut resolved,
        cwd,
        args,
        mut environment,
        ..
    } = prepared;
    if let Some(argv) = wrapped_argv {
        let command = argv
            .first()
            .map_or_else(|| resolved.executable.clone(), std::path::PathBuf::from);
        let launcher = agentshim_core::tools::exec::resolve::launcher_for(&command)
            .map_err(|error| Error::new(napi::Status::GenericFailure, error.to_string()))?;
        resolved = agentshim_core::tools::exec::resolve::ResolvedProgram {
            absolute: command.clone(),
            executable: command,
            launcher,
        };
        let final_args = if argv.len() > 1 {
            argv[1..].to_vec()
        } else {
            Vec::new()
        };
        return Ok(PreparedBackgroundBash {
            resolved,
            cwd,
            args: final_args,
            environment,
        });
    }
    environment.base = Some(state.env.clone());
    Ok(PreparedBackgroundBash {
        resolved,
        cwd,
        args,
        environment,
    })
}
