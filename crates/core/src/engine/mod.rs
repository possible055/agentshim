use std::{io, sync::Arc, time::Instant};

use tokio::sync::OwnedSemaphorePermit;
use tokio_util::sync::CancellationToken;

use crate::{
    output::CallBudget,
    path::{FileAccess, ReadScope, RepositoryRoot},
    runtime::{MemoryReservation, RuntimeResources},
    tools::{
        ToolOutput,
        bash::locate::BashLocator,
        exec::{ProcessError, resolve::ProcessResolver},
        glob, grep, read,
    },
};

#[derive(Clone)]
pub struct OperationContext {
    cancellation: CancellationToken,
    output_budget: Arc<dyn CallBudget>,
}

impl OperationContext {
    #[must_use]
    pub fn new(cancellation: CancellationToken, output_budget: Arc<dyn CallBudget>) -> Self {
        Self {
            cancellation,
            output_budget,
        }
    }
}

#[derive(Clone)]
pub struct ToolEngine {
    root: Arc<RepositoryRoot>,
    access: Arc<FileAccess>,
    resources: RuntimeResources,
    process_resolver: ProcessResolver,
    bash_locator: BashLocator,
    process_environment: Option<ProcessEnvironment>,
}

pub use process::{PreparedBash, PreparedRunProgram, ProcessEnvironment};

mod process;

impl ToolEngine {
    #[must_use]
    pub fn new(
        root: Arc<RepositoryRoot>,
        read_scope: ReadScope,
        resources: RuntimeResources,
    ) -> Self {
        let access = Arc::new(FileAccess::new(Arc::clone(&root), read_scope));
        Self {
            root,
            access,
            resources,
            process_resolver: ProcessResolver::capture(),
            bash_locator: BashLocator::capture(),
            process_environment: None,
        }
    }

    /// Configure the explicit base environment used by child processes and the optional
    /// Bash executable override resolved from that same environment.
    #[must_use]
    pub fn with_process_environment(mut self, environment: ProcessEnvironment) -> Self {
        self.bash_locator = BashLocator::capture_with_override(environment.bash_override.clone());
        self.process_environment = Some(environment);
        self
    }

    /// Clone this engine with a narrower file capability derived from the same root.
    ///
    /// # Errors
    ///
    /// Returns invalid input when the access capability belongs to another repository root.
    pub fn with_file_access(&self, access: Arc<FileAccess>) -> io::Result<Self> {
        if access.root().path() != self.root.path() || access.scope() != self.access.scope() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "file access must preserve the ToolEngine repository root and read scope",
            ));
        }
        Ok(Self {
            root: Arc::clone(&self.root),
            access,
            resources: self.resources.clone(),
            process_resolver: self.process_resolver.clone(),
            bash_locator: self.bash_locator.clone(),
            process_environment: self.process_environment.clone(),
        })
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the two-attempt read transaction keeps one PDF gate, timeout, and output lease"
    )]
    pub async fn read(
        &self,
        request: read::ReadRequest,
        context: OperationContext,
    ) -> Result<ToolOutput, read::ReadError> {
        let queued = Instant::now();
        let admission = self
            .try_read_only_admission(&context.cancellation)
            .ok_or_else(|| {
                cancelled_or_busy(
                    read::ReadError::Cancelled,
                    read::ReadError::ResourceBusy {
                        resource: "read_only",
                        retry_after: None,
                    },
                    &context.cancellation,
                    &self.resources,
                )
            })?;
        let worker = self
            .resources
            .acquire_worker(&context.cancellation)
            .await
            .map_err(|_| read::ReadError::Cancelled)?;
        let open_file = self
            .resources
            .acquire_open_file(&context.cancellation)
            .await
            .map_err(|_| read::ReadError::Cancelled)?;
        trace_capacity_acquired(queued);
        let (cancellation, _relay) =
            relayed_cancellation(&context.cancellation, self.resources.shutdown_token());
        let budgets = read::PdfMemoryBudgets::from_config(&self.resources.config());
        let timed_out = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut pdf_admission: Option<(OwnedSemaphorePermit, OwnedSemaphorePermit)> = None;
        let mut deadline = None;
        let mut result = None;
        let span = tracing::Span::current();

        for attempt_index in 0..2 {
            let access = Arc::clone(&self.access);
            let prepare_request = request.clone();
            let prepare_cancellation = cancellation.clone();
            let prepare_span = span.clone();
            let prepared = tokio::task::spawn_blocking(move || {
                prepare_span.in_scope(|| {
                    read::prepare(&access, &prepare_request, &prepare_cancellation, budgets)
                })
            })
            .await
            .map_err(|error| read::ReadError::Worker(error.to_string()))?;
            let prepared = match prepared {
                Ok(prepared) => prepared,
                Err(read::ReadError::Io(error))
                    if attempt_index > 0 && error.kind() == io::ErrorKind::NotFound =>
                {
                    result = Some(Err(read::ReadError::Changed));
                    break;
                }
                Err(error) => {
                    result = Some(Err(normalize_cancellation(error, &cancellation)));
                    break;
                }
            };

            let mut text_memory = None;
            if prepared.pdf_mode().is_some() {
                if pdf_admission.is_none() {
                    pdf_admission =
                        Some(self.acquire_pdf_admission(&prepared, &cancellation).await?);
                }
                deadline = prepared.runtime_limit();
            } else {
                text_memory = Some(
                    self.resources
                        .reserve_memory(prepared.memory_charge(), &cancellation)
                        .await
                        .map_err(|_| read::ReadError::Cancelled)?,
                );
            }

            let access = Arc::clone(&self.access);
            let execute_request = request.clone();
            let execute_cancellation = cancellation.clone();
            let output_budget = Arc::clone(&context.output_budget);
            let execute_span = span.clone();
            let started = Instant::now();
            let timer = deadline.map(|limit| {
                let token = cancellation.clone();
                let expired = Arc::clone(&timed_out);
                tokio::spawn(async move {
                    tokio::time::sleep(limit).await;
                    expired.store(true, std::sync::atomic::Ordering::SeqCst);
                    token.cancel();
                })
            });
            let executed = tokio::task::spawn_blocking(move || {
                execute_span.in_scope(|| {
                    read::execute_prepared_with_budget(
                        &access,
                        &execute_request,
                        prepared,
                        &execute_cancellation,
                        output_budget.as_ref(),
                    )
                })
            })
            .await;
            if let Some(timer) = timer {
                timer.abort();
            }
            let executed = executed.map_err(|error| read::ReadError::Worker(error.to_string()))?;
            drop(text_memory);

            let elapsed = started.elapsed();
            if timed_out.load(std::sync::atomic::Ordering::SeqCst)
                || deadline.is_some_and(|limit| elapsed > limit)
            {
                return Err(read::ReadError::ResourceTimeout {
                    limit: deadline.unwrap_or_default(),
                    elapsed,
                });
            }
            match executed {
                Ok(read::Attempt::Stable(output)) => {
                    result = Some(Ok(output));
                    break;
                }
                Ok(read::Attempt::Changed) if attempt_index == 0 => {
                    tracing::warn!(target: "agentshim", event = "read_retry", phase = "execution", outcome = "degraded_success", reason = "file_changed");
                }
                Ok(read::Attempt::Changed) => {
                    result = Some(Err(read::ReadError::Changed));
                    break;
                }
                Err(error) => {
                    result = Some(Err(normalize_cancellation(error, &cancellation)));
                    break;
                }
            }
        }

        drop((admission, worker, open_file));
        let output = result.expect("read attempt loop always produces a result")?;
        Ok(if let Some((gate, memory)) = pdf_admission {
            output.retain_resources(crate::runtime::resources::OutputLease::new(vec![
                gate, memory,
            ]))
        } else {
            output
        })
    }

    pub async fn glob(
        &self,
        request: glob::GlobRequest,
        context: OperationContext,
    ) -> Result<ToolOutput, glob::GlobError> {
        let admission = self
            .try_read_only_admission(&context.cancellation)
            .ok_or_else(|| {
                cancelled_or_busy(
                    glob::GlobError::Cancelled,
                    glob::GlobError::ResourceBusy("read_only"),
                    &context.cancellation,
                    &self.resources,
                )
            })?;
        let charge = glob::memory_charge(&request);
        let access = Arc::clone(&self.access);
        let resources = self.resources.clone();
        let span = tracing::Span::current();
        self.spawn_budgeted_search(
            &context,
            charge,
            vec![admission],
            || glob::GlobError::Cancelled,
            glob::GlobError::Worker,
            move |cancellation, output_budget, reservation| {
                span.in_scope(|| {
                    glob::execute_output_with_budget(
                        &access,
                        &request,
                        &resources,
                        cancellation,
                        reservation,
                        output_budget,
                    )
                    .map_err(|error| normalize_cancellation(error, cancellation))
                })
            },
        )
        .await
    }

    pub async fn grep(
        &self,
        request: grep::GrepRequest,
        context: OperationContext,
    ) -> Result<ToolOutput, grep::GrepError> {
        let grep_admission = self.resources.try_admit_grep().ok_or_else(|| {
            cancelled_or_busy(
                grep::GrepError::Cancelled,
                grep::GrepError::ResourceBusy("grep_concurrency"),
                &context.cancellation,
                &self.resources,
            )
        })?;
        let admission = self
            .try_read_only_admission(&context.cancellation)
            .ok_or_else(|| {
                cancelled_or_busy(
                    grep::GrepError::Cancelled,
                    grep::GrepError::ResourceBusy("read_only"),
                    &context.cancellation,
                    &self.resources,
                )
            })?;
        let open_file = self
            .resources
            .acquire_open_file(&context.cancellation)
            .await
            .map_err(|_| grep::GrepError::Cancelled)?;
        let charge = grep::base_memory_charge(self.resources.config().grep_memory_bytes);
        let access = Arc::clone(&self.access);
        let resources = self.resources.clone();
        let span = tracing::Span::current();
        self.spawn_budgeted_search(
            &context,
            charge,
            vec![grep_admission, admission, open_file],
            || grep::GrepError::Cancelled,
            grep::GrepError::Worker,
            move |cancellation, output_budget, reservation| {
                span.in_scope(|| {
                    grep::execute_output_with_budget(
                        &access,
                        &request,
                        &resources,
                        cancellation,
                        reservation,
                        output_budget,
                    )
                    .map_err(|error| normalize_cancellation(error, cancellation))
                })
            },
        )
        .await
    }

    async fn acquire_pdf_admission(
        &self,
        prepared: &read::PreparedRead,
        cancellation: &CancellationToken,
    ) -> Result<(OwnedSemaphorePermit, OwnedSemaphorePermit), read::ReadError> {
        let Some(gate) = self.resources.acquire_pdf_gate(cancellation).await else {
            if cancellation.is_cancelled() || self.resources.shutdown_token().is_cancelled() {
                return Err(read::ReadError::Cancelled);
            }
            return Err(read::ReadError::ResourceBusy {
                resource: "pdf_concurrency",
                retry_after: Some(crate::runtime::PDF_GATE_WAIT),
            });
        };
        let Some(memory) = self.resources.try_reserve_memory(prepared.memory_charge()) else {
            return Err(read::ReadError::ResourceBusy {
                resource: "memory_budget",
                retry_after: Some(crate::runtime::PDF_GATE_WAIT),
            });
        };
        Ok((gate, memory))
    }

    fn try_read_only_admission(
        &self,
        cancellation: &CancellationToken,
    ) -> Option<OwnedSemaphorePermit> {
        if cancellation.is_cancelled() {
            return None;
        }
        self.resources.try_admit_read_only()
    }

    /// One admission-to-worker pipeline for the read-only search tools: admit the
    /// caller's permits, take a worker and the tool's memory charge, then run the
    /// search on the blocking pool under a shutdown-relayed cancellation token.
    #[allow(clippy::type_complexity)]
    async fn spawn_budgeted_search<E, F>(
        &self,
        context: &OperationContext,
        charge: usize,
        mut holds: Vec<OwnedSemaphorePermit>,
        cancelled: fn() -> E,
        worker_error: fn(String) -> E,
        execute: F,
    ) -> Result<ToolOutput, E>
    where
        E: Send + 'static,
        F: FnOnce(&CancellationToken, &dyn CallBudget, MemoryReservation) -> Result<ToolOutput, E>
            + Send
            + 'static,
    {
        let queued = Instant::now();
        let worker = self
            .resources
            .acquire_worker(&context.cancellation)
            .await
            .map_err(|_| cancelled())?;
        let memory = self
            .resources
            .reserve_memory(charge, &context.cancellation)
            .await
            .map_err(|_| cancelled())?;
        trace_capacity_acquired(queued);
        holds.push(worker);
        let reservation = MemoryReservation::from_initial(&self.resources, memory, charge);
        let output_budget = Arc::clone(&context.output_budget);
        self.relayed_blocking(context, worker_error, move |cancellation| {
            let result = execute(cancellation, output_budget.as_ref(), reservation);
            drop(holds);
            result
        })
        .await
    }

    async fn spawn_prepared_process<F>(
        &self,
        deadline: std::time::Instant,
        timeout_ms: u64,
        memory_charge: usize,
        context: &OperationContext,
        execute: F,
    ) -> Result<ToolOutput, ProcessError>
    where
        F: FnOnce(&CancellationToken, &dyn CallBudget) -> Result<ToolOutput, ProcessError>
            + Send
            + 'static,
    {
        let permits = self
            .acquire_process(deadline, timeout_ms, memory_charge, &context.cancellation)
            .await?;
        let output_budget = Arc::clone(&context.output_budget);
        self.relayed_blocking(context, ProcessError::Worker, move |cancellation| {
            let result = execute(cancellation, output_budget.as_ref());
            drop(permits);
            result
        })
        .await
    }

    /// Run `work` on the blocking pool with the request's cancellation relayed to the
    /// shutdown token, mapping a pool worker panic to the caller's error type.
    async fn relayed_blocking<T, E>(
        &self,
        context: &OperationContext,
        worker_error: fn(String) -> E,
        work: impl FnOnce(&CancellationToken) -> Result<T, E> + Send + 'static,
    ) -> Result<T, E>
    where
        T: Send + 'static,
        E: Send + 'static,
    {
        let (cancellation, _relay) =
            relayed_cancellation(&context.cancellation, self.resources.shutdown_token());
        match tokio::task::spawn_blocking(move || work(&cancellation)).await {
            Ok(result) => result,
            Err(error) => Err(worker_error(error.to_string())),
        }
    }

    pub async fn auxiliary_read_only<T, F>(
        &self,
        memory_bytes: usize,
        cancellation: CancellationToken,
        work: F,
    ) -> Result<T, AuxiliaryError>
    where
        T: Send + 'static,
        F: FnOnce(CancellationToken) -> T + Send + 'static,
    {
        let queued = Instant::now();
        let admission = self.try_read_only_admission(&cancellation).ok_or_else(|| {
            if cancellation.is_cancelled() || self.resources.shutdown_token().is_cancelled() {
                AuxiliaryError::Cancelled
            } else {
                AuxiliaryError::Busy
            }
        })?;
        let worker = self
            .resources
            .acquire_worker(&cancellation)
            .await
            .map_err(|_| AuxiliaryError::Cancelled)?;
        let memory = self
            .resources
            .reserve_memory(memory_bytes, &cancellation)
            .await
            .map_err(|_| AuxiliaryError::Cancelled)?;
        trace_capacity_acquired(queued);
        let (cancellation, _relay) =
            relayed_cancellation(&cancellation, self.resources.shutdown_token());
        tokio::task::spawn_blocking(move || {
            let result = work(cancellation);
            drop((admission, worker, memory));
            result
        })
        .await
        .map_err(|error| AuxiliaryError::Worker(error.to_string()))
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub fn try_acquire_pdf_gate_for_test(&self) -> Option<OwnedSemaphorePermit> {
        self.resources.try_acquire_pdf_gate()
    }

    #[cfg(any(test, feature = "test-hooks"))]
    #[must_use]
    pub fn available_pdf_slots_for_test(&self) -> usize {
        self.resources.available_pdf_slots()
    }

    #[cfg(any(test, feature = "test-hooks"))]
    #[must_use]
    pub fn pdf_gate_acquisitions_for_test(&self) -> usize {
        self.resources.pdf_gate_acquisitions()
    }

    #[cfg(any(test, feature = "test-hooks"))]
    #[must_use]
    pub fn available_memory_bytes_for_test(&self) -> usize {
        self.resources.available_memory_bytes()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AuxiliaryError {
    #[error("read-only capacity is busy")]
    Busy,
    #[error("auxiliary read-only work was cancelled")]
    Cancelled,
    #[error("auxiliary read-only worker failed: {0}")]
    Worker(String),
}

struct CancellationRelay(tokio::task::JoinHandle<()>);

impl Drop for CancellationRelay {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn relayed_cancellation(
    request: &CancellationToken,
    shutdown: CancellationToken,
) -> (CancellationToken, CancellationRelay) {
    let cancellation = request.child_token();
    if shutdown.is_cancelled() {
        cancellation.cancel();
    }
    let signal = cancellation.clone();
    let relay = tokio::spawn(async move {
        shutdown.cancelled().await;
        signal.cancel();
    });
    (cancellation, CancellationRelay(relay))
}

pub(super) fn trace_capacity_acquired(queued: Instant) {
    tracing::info!(
        target: "agentshim",
        event = "capacity_acquired",
        phase = "queue",
        queue_ms = u64::try_from(queued.elapsed().as_millis()).unwrap_or(u64::MAX)
    );
}

fn cancelled_or_busy<E>(
    cancelled: E,
    busy: E,
    request: &CancellationToken,
    resources: &RuntimeResources,
) -> E {
    if request.is_cancelled() || resources.shutdown_token().is_cancelled() {
        cancelled
    } else {
        busy
    }
}

/// Error types whose cancellation-shaped variants must collapse into the single
/// `Cancelled` variant when the relayed token actually fired.
trait CancellationClassified: Sized {
    fn cancellation_shaped(&self) -> bool;
    fn cancelled() -> Self;
}

impl CancellationClassified for read::ReadError {
    fn cancellation_shaped(&self) -> bool {
        match self {
            read::ReadError::Output(crate::output::OutputError::Cancelled)
            | read::ReadError::Cancelled => true,
            read::ReadError::Pdf(error) => {
                error.kind() == agentshim_pdf_read::PdfReadErrorKind::Cancelled
            }
            _ => false,
        }
    }

    fn cancelled() -> Self {
        read::ReadError::Cancelled
    }
}

impl CancellationClassified for glob::GlobError {
    fn cancellation_shaped(&self) -> bool {
        matches!(
            self,
            glob::GlobError::Traversal(crate::traversal::TraversalError::Cancelled)
                | glob::GlobError::Output(crate::output::OutputError::Cancelled)
                | glob::GlobError::Cancelled
        )
    }

    fn cancelled() -> Self {
        glob::GlobError::Cancelled
    }
}

impl CancellationClassified for grep::GrepError {
    fn cancellation_shaped(&self) -> bool {
        matches!(
            self,
            grep::GrepError::Traversal(crate::traversal::TraversalError::Cancelled)
                | grep::GrepError::Output(crate::output::OutputError::Cancelled)
                | grep::GrepError::Cancelled
        )
    }

    fn cancelled() -> Self {
        grep::GrepError::Cancelled
    }
}

fn normalize_cancellation<E: CancellationClassified>(
    error: E,
    cancellation: &CancellationToken,
) -> E {
    if cancellation.is_cancelled() && error.cancellation_shaped() {
        E::cancelled()
    } else {
        error
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, sync::Arc, time::Duration};

    use tokio_util::sync::CancellationToken;

    use super::{OperationContext, ToolEngine};
    use crate::{
        output::TestCallBudget,
        path::{ReadScope, RepositoryRoot},
        runtime::{MAX_READ_ONLY_CALLS, RuntimeCapacity, RuntimeConfig, RuntimeResources},
        tools::{
            exec::ProcessError, glob::GlobRequest, grep::GrepRequest, read::ReadRequest,
            run_program::ProcessRequest,
        },
    };

    fn context() -> OperationContext {
        OperationContext::new(
            CancellationToken::new(),
            Arc::new(TestCallBudget::default()),
        )
    }

    fn read_request(path: &str) -> ReadRequest {
        ReadRequest {
            path: path.to_owned(),
            start_line: None,
            line_count: None,
            encoding: None,
            pdf_mode: None,
            pages: None,
            pdf_cursor: None,
        }
    }

    #[tokio::test]
    async fn facade_executes_read_glob_and_grep_with_public_results() {
        let fixture = tempfile::tempdir().expect("fixture");
        std::fs::write(fixture.path().join("notes.md"), "alpha needle\nbravo\n")
            .expect("fixture file");
        let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
        let engine = ToolEngine::new(
            Arc::clone(&root),
            ReadScope::Normal,
            RuntimeResources::new(RuntimeConfig::for_tests(2)),
        );
        let elevated = Arc::new(crate::path::FileAccess::new(root, ReadScope::Unrestricted));
        assert!(engine.with_file_access(elevated).is_err());

        let read = engine
            .read(read_request("notes.md"), context())
            .await
            .expect("read");
        assert!(read.text.contains("alpha needle"));

        let glob = engine
            .glob(
                GlobRequest {
                    pattern: "*.md".to_owned(),
                    path: None,
                    include_ignored: None,
                    entry_type: None,
                    offset: None,
                    limit: None,
                },
                context(),
            )
            .await
            .expect("glob");
        assert!(glob.text.contains("notes.md"));

        let grep = engine
            .grep(
                GrepRequest {
                    pattern: "needle".to_owned(),
                    path: Some(".".to_owned()),
                    glob: None,
                    mode: None,
                    fixed_strings: Some(true),
                    case: None,
                    context_lines: None,
                    offset: None,
                    limit: None,
                    include_ignored: None,
                    encoding: None,
                    fallback_encoding: None,
                },
                context(),
            )
            .await
            .expect("grep");
        assert!(grep.text.contains("notes.md"));
    }

    #[tokio::test]
    async fn shared_capacity_does_not_share_engine_cancellation() {
        let fixture = tempfile::tempdir().expect("fixture");
        std::fs::write(fixture.path().join("notes.md"), "alpha\n").expect("fixture file");
        let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
        let capacity = Arc::new(RuntimeCapacity::new(RuntimeConfig::for_tests(1)));
        let first_shutdown = CancellationToken::new();
        let first_resources =
            RuntimeResources::from_capacity(Arc::clone(&capacity), first_shutdown.clone());
        let second_resources = RuntimeResources::from_capacity(capacity, CancellationToken::new());
        let first = ToolEngine::new(
            Arc::clone(&root),
            ReadScope::Normal,
            first_resources.clone(),
        );
        let second = ToolEngine::new(root, ReadScope::Normal, second_resources);

        let occupied = (0..MAX_READ_ONLY_CALLS)
            .map(|_| {
                first_resources
                    .try_admit_read_only()
                    .expect("read-only fixture admission")
            })
            .collect::<Vec<_>>();
        assert!(matches!(
            second.read(read_request("notes.md"), context()).await,
            Err(crate::tools::read::ReadError::ResourceBusy {
                resource: "read_only",
                ..
            })
        ));
        drop(occupied);

        first_shutdown.cancel();
        assert!(matches!(
            first.read(read_request("notes.md"), context()).await,
            Err(crate::tools::read::ReadError::Cancelled)
        ));
        assert!(
            second
                .read(read_request("notes.md"), context())
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn request_cancellation_stops_capacity_wait_and_releases_admission() {
        let fixture = tempfile::tempdir().expect("fixture");
        std::fs::write(fixture.path().join("notes.md"), "alpha\n").expect("fixture file");
        let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
        let resources = RuntimeResources::new(RuntimeConfig::for_tests(1));
        let worker = resources
            .try_acquire_worker()
            .expect("occupied worker fixture");
        let engine = ToolEngine::new(root, ReadScope::Normal, resources.clone());
        let cancellation = CancellationToken::new();
        let worker_cancellation = cancellation.clone();
        let pending = tokio::spawn(async move {
            engine
                .read(
                    read_request("notes.md"),
                    OperationContext::new(worker_cancellation, Arc::new(TestCallBudget::default())),
                )
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !resources.has_in_flight_calls() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("read entered capacity wait");
        cancellation.cancel();
        assert!(matches!(
            pending.await.expect("read task"),
            Err(crate::tools::read::ReadError::Cancelled)
        ));
        assert!(!resources.has_in_flight_calls());
        drop(worker);
    }

    #[tokio::test]
    async fn process_prepare_holds_no_slot_and_spawn_enforces_capacity_and_deadline() {
        let fixture = tempfile::tempdir().expect("fixture");
        let root = Arc::new(RepositoryRoot::open(fixture.path()).expect("root"));
        let mut config = RuntimeConfig::for_tests(1);
        config.process_calls = 1;
        let resources = RuntimeResources::new(config);
        let engine = ToolEngine::new(root, ReadScope::Normal, resources.clone());
        let request = |timeout_ms| ProcessRequest {
            program: std::env::current_exe()
                .expect("current test executable")
                .to_string_lossy()
                .into_owned(),
            args: vec!["--list".to_owned()],
            cwd: None,
            env: BTreeMap::new(),
            unset_env: Vec::new(),
            stdin: None,
            timeout_ms: Some(timeout_ms),
        };
        let occupied = resources
            .try_admit_process_for_test()
            .expect("occupied process slot");
        let prepared = engine
            .prepare_run_program(&request(1_000), 10_000, &context())
            .expect("prepare does not need process admission");
        assert!(matches!(
            engine
                .spawn_run_program(prepared, None, context(), None)
                .await,
            Err(ProcessError::ResourceBusy(_))
        ));
        drop(occupied);

        let prepared = engine
            .prepare_run_program(&request(1), 10_000, &context())
            .expect("short prepare");
        std::thread::sleep(Duration::from_millis(10));
        assert!(matches!(
            engine
                .spawn_run_program(prepared, None, context(), None)
                .await,
            Err(ProcessError::TimeoutBeforeSpawn { timeout_ms: 1 })
        ));
    }
}
