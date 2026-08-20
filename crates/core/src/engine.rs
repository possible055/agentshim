use std::{
    ffi::OsString,
    io,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use tokio::sync::OwnedSemaphorePermit;
use tokio_util::sync::CancellationToken;

use crate::{
    output::CallBudget,
    path::{FileAccess, ReadScope, RepositoryRoot},
    runtime::{MemoryReservation, RuntimeResources},
    tools::{
        ToolOutput,
        bash::{
            self,
            detached::DetachedAdmission,
            locate::{BashLocator, LocateError},
        },
        exec::{
            CaptureSink, ProcessError,
            resolve::{ProcessResolver, ResolvedProgram, launcher_for},
            spawn::{EnvironmentPlan, ExecPlan, Streams},
        },
        glob, grep, read, run_program,
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

#[derive(Clone, Debug)]
pub struct ProcessEnvironment {
    entries: Arc<Vec<(String, String)>>,
    bash_override: Option<OsString>,
}

impl ProcessEnvironment {
    pub fn new(
        entries: Vec<(String, String)>,
        bash_override: Option<OsString>,
    ) -> Result<Self, ProcessError> {
        run_program::validate_base_environment(&entries)?;
        Ok(Self {
            entries: Arc::new(entries),
            bash_override,
        })
    }
}

#[derive(Debug)]
pub struct PreparedRunProgram {
    inner: run_program::PreparedRunProgram,
    memory_charge: usize,
}

impl PreparedRunProgram {
    #[must_use]
    pub fn argv(&self) -> Vec<String> {
        prepared_argv(&self.inner.resolved, &self.inner.args)
    }
}

#[derive(Debug)]
pub struct PreparedBash {
    inner: bash::PreparedBash,
    memory_charge: usize,
    background_timeout: Option<Duration>,
}

impl PreparedBash {
    #[must_use]
    pub fn argv(&self) -> Vec<String> {
        prepared_argv(&self.inner.resolved, &self.inner.args)
    }
}

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
            .ok_or_else(|| cancelled_or_read_busy(&context.cancellation, &self.resources))?;
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
                    result = Some(Err(normalize_read_cancellation(error, &cancellation)));
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
                    result = Some(Err(normalize_read_cancellation(error, &cancellation)));
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
        let queued = Instant::now();
        let admission = self
            .try_read_only_admission(&context.cancellation)
            .ok_or_else(|| cancelled_or_glob_busy(&context.cancellation, &self.resources))?;
        let worker = self
            .resources
            .acquire_worker(&context.cancellation)
            .await
            .map_err(|_| glob::GlobError::Cancelled)?;
        let charge = glob::memory_charge(&request);
        let memory = self
            .resources
            .reserve_memory(charge, &context.cancellation)
            .await
            .map_err(|_| glob::GlobError::Cancelled)?;
        trace_capacity_acquired(queued);
        let reservation = MemoryReservation::from_initial(&self.resources, memory, charge);
        let (cancellation, _relay) =
            relayed_cancellation(&context.cancellation, self.resources.shutdown_token());
        let access = Arc::clone(&self.access);
        let resources = self.resources.clone();
        let output_budget = Arc::clone(&context.output_budget);
        let span = tracing::Span::current();
        tokio::task::spawn_blocking(move || {
            span.in_scope(|| {
                let result = glob::execute_output_with_budget(
                    &access,
                    &request,
                    &resources,
                    &cancellation,
                    reservation,
                    output_budget.as_ref(),
                );
                drop((admission, worker));
                result.map_err(|error| normalize_glob_cancellation(error, &cancellation))
            })
        })
        .await
        .map_err(|error| glob::GlobError::Worker(error.to_string()))?
    }

    pub async fn grep(
        &self,
        request: grep::GrepRequest,
        context: OperationContext,
    ) -> Result<ToolOutput, grep::GrepError> {
        let queued = Instant::now();
        let grep_admission = self.resources.try_admit_grep().ok_or_else(|| {
            cancelled_or_grep_concurrency_busy(&context.cancellation, &self.resources)
        })?;
        let admission = self
            .try_read_only_admission(&context.cancellation)
            .ok_or_else(|| cancelled_or_grep_busy(&context.cancellation, &self.resources))?;
        let worker = self
            .resources
            .acquire_worker(&context.cancellation)
            .await
            .map_err(|_| grep::GrepError::Cancelled)?;
        let open_file = self
            .resources
            .acquire_open_file(&context.cancellation)
            .await
            .map_err(|_| grep::GrepError::Cancelled)?;
        let charge = grep::base_memory_charge(self.resources.config().grep_memory_bytes);
        let memory = self
            .resources
            .reserve_memory(charge, &context.cancellation)
            .await
            .map_err(|_| grep::GrepError::Cancelled)?;
        trace_capacity_acquired(queued);
        let reservation = MemoryReservation::from_initial(&self.resources, memory, charge);
        let (cancellation, _relay) =
            relayed_cancellation(&context.cancellation, self.resources.shutdown_token());
        let access = Arc::clone(&self.access);
        let resources = self.resources.clone();
        let output_budget = Arc::clone(&context.output_budget);
        let span = tracing::Span::current();
        tokio::task::spawn_blocking(move || {
            span.in_scope(|| {
                let result = grep::execute_output_with_budget(
                    &access,
                    &request,
                    &resources,
                    &cancellation,
                    reservation,
                    output_budget.as_ref(),
                );
                drop((grep_admission, admission, worker, open_file));
                result.map_err(|error| normalize_grep_cancellation(error, &cancellation))
            })
        })
        .await
        .map_err(|error| grep::GrepError::Worker(error.to_string()))?
    }

    pub fn prepare_run_program(
        &self,
        request: &run_program::ProcessRequest,
        timeout_ceiling_ms: u64,
        context: &OperationContext,
    ) -> Result<PreparedRunProgram, ProcessError> {
        self.ensure_process_active(&context.cancellation)?;
        let timeout = Duration::from_millis(request.timeout_ms(timeout_ceiling_ms));
        let mut inner = run_program::prepare_run_program(
            &self.root,
            &self.process_resolver,
            request,
            timeout,
            timeout_ceiling_ms,
        )?;
        self.apply_process_environment(&mut inner.environment);
        Ok(PreparedRunProgram {
            inner,
            memory_charge: request.memory_charge(),
        })
    }

    pub fn prepare_bash(
        &self,
        request: &bash::BashRequest,
        timeout_ceiling_ms: u64,
        context: &OperationContext,
    ) -> Result<PreparedBash, ProcessError> {
        self.ensure_process_active(&context.cancellation)?;
        if request.detach {
            return Err(ProcessError::Validation(
                "foreground bash prepare does not accept detach".to_owned(),
            ));
        }
        let timeout = Duration::from_millis(request.timeout_ms(timeout_ceiling_ms));
        let mut inner = bash::prepare_bash_foreground(
            &self.root,
            &self.bash_locator,
            request,
            timeout,
            timeout_ceiling_ms,
            &context.cancellation,
        )?;
        self.apply_process_environment(&mut inner.environment);
        Ok(PreparedBash {
            inner,
            memory_charge: request.memory_charge(),
            background_timeout: None,
        })
    }

    pub fn prepare_background_bash(
        &self,
        request: &bash::BashRequest,
        background_timeout_max_ms: u64,
        launch_timeout_ceiling_ms: u64,
        context: &OperationContext,
    ) -> Result<PreparedBash, ProcessError> {
        self.ensure_process_active(&context.cancellation)?;
        if request.detach {
            return Err(ProcessError::Validation(
                "native background bash prepare does not accept detach".to_owned(),
            ));
        }
        request.validate(background_timeout_max_ms)?;
        let launch_timeout_ms =
            crate::tools::exec::spawn::default_timeout_within(launch_timeout_ceiling_ms);
        let mut inner = bash::prepare_bash_foreground(
            &self.root,
            &self.bash_locator,
            request,
            Duration::from_millis(launch_timeout_ms),
            background_timeout_max_ms,
            &context.cancellation,
        )?;
        inner.request_timeout_ms = launch_timeout_ms;
        self.apply_process_environment(&mut inner.environment);
        Ok(PreparedBash {
            inner,
            memory_charge: request.memory_charge(),
            background_timeout: Some(Duration::from_millis(
                request.background_timeout_ms(background_timeout_max_ms),
            )),
        })
    }

    pub async fn spawn_run_program(
        &self,
        prepared: PreparedRunProgram,
        wrapped_argv: Option<Vec<String>>,
        context: OperationContext,
        capture_sink: Option<Arc<dyn CaptureSink>>,
    ) -> Result<ToolOutput, ProcessError> {
        let permits = self
            .acquire_process(
                prepared.inner.deadline,
                prepared.inner.request_timeout_ms,
                prepared.memory_charge,
                &context.cancellation,
            )
            .await?;
        let (cancellation, _relay) =
            relayed_cancellation(&context.cancellation, self.resources.shutdown_token());
        let output_budget = Arc::clone(&context.output_budget);
        let span = tracing::Span::current();
        tokio::task::spawn_blocking(move || {
            span.in_scope(|| {
                let result = run_program::execute_prepared_run_program(
                    prepared.inner,
                    wrapped_argv.as_deref(),
                    &cancellation,
                    output_budget.as_ref(),
                    capture_sink.as_ref(),
                );
                drop(permits);
                result
            })
        })
        .await
        .map_err(|error| ProcessError::Worker(error.to_string()))?
    }

    pub async fn spawn_bash(
        &self,
        prepared: PreparedBash,
        wrapped_argv: Option<Vec<String>>,
        context: OperationContext,
        capture_sink: Option<Arc<dyn CaptureSink>>,
    ) -> Result<ToolOutput, ProcessError> {
        let permits = self
            .acquire_process(
                prepared.inner.deadline,
                prepared.inner.request_timeout_ms,
                prepared.memory_charge,
                &context.cancellation,
            )
            .await?;
        let (cancellation, _relay) =
            relayed_cancellation(&context.cancellation, self.resources.shutdown_token());
        let output_budget = Arc::clone(&context.output_budget);
        let span = tracing::Span::current();
        tokio::task::spawn_blocking(move || {
            span.in_scope(|| {
                let result = bash::execute_prepared_bash(
                    prepared.inner,
                    wrapped_argv.as_deref(),
                    &cancellation,
                    output_budget.as_ref(),
                    capture_sink.as_ref(),
                );
                drop(permits);
                result
            })
        })
        .await
        .map_err(|error| ProcessError::Worker(error.to_string()))?
    }

    pub async fn run_detached_bash<F>(
        &self,
        request: bash::BashRequest,
        admission: DetachedAdmission,
        background_timeout_max_ms: u64,
        launch_timeout_ceiling_ms: u64,
        on_commit: F,
        context: OperationContext,
    ) -> Result<ToolOutput, ProcessError>
    where
        F: FnOnce() + Send + 'static,
    {
        if !request.detach {
            return Err(ProcessError::Validation(
                "detached bash execution requires detach".to_owned(),
            ));
        }
        request.validate(background_timeout_max_ms)?;
        self.ensure_process_active(&context.cancellation)?;
        let launch_timeout_ms =
            crate::tools::exec::spawn::default_timeout_within(launch_timeout_ceiling_ms);
        let launch_timeout = Duration::from_millis(launch_timeout_ms);
        let queued = Instant::now();
        let memory = tokio::time::timeout(
            launch_timeout,
            self.resources
                .reserve_memory(request.memory_charge(), &context.cancellation),
        )
        .await
        .map_err(|_| ProcessError::TimeoutBeforeSpawn {
            timeout_ms: launch_timeout_ms,
        })?
        .map_err(|_| ProcessError::Cancelled)?;
        trace_capacity_acquired(queued);
        let remaining = launch_timeout
            .checked_sub(queued.elapsed())
            .filter(|remaining| !remaining.is_zero())
            .ok_or(ProcessError::TimeoutBeforeSpawn {
                timeout_ms: launch_timeout_ms,
            })?;
        let root = Arc::clone(&self.root);
        let locator = self.bash_locator.clone();
        let (cancellation, _relay) =
            relayed_cancellation(&context.cancellation, self.resources.shutdown_token());
        let output_budget = Arc::clone(&context.output_budget);
        let span = tracing::Span::current();
        tokio::task::spawn_blocking(move || {
            span.in_scope(|| {
                let result = bash::execute_output_with_capture(
                    &root,
                    &locator,
                    Some(admission),
                    &request,
                    remaining,
                    &cancellation,
                    background_timeout_max_ms,
                    output_budget.as_ref(),
                    None,
                );
                if result.is_ok() {
                    on_commit();
                }
                drop(memory);
                result
            })
        })
        .await
        .map_err(|error| ProcessError::Worker(error.to_string()))?
    }

    pub fn spawn_background_bash(
        &self,
        prepared: PreparedBash,
        wrapped_argv: Option<&[String]>,
        context: &OperationContext,
    ) -> Result<
        (
            crate::platform::process::DetachedTree,
            std::fs::File,
            Duration,
            Instant,
        ),
        ProcessError,
    > {
        self.ensure_process_active(&context.cancellation)?;
        let background_timeout = prepared.background_timeout.ok_or_else(|| {
            ProcessError::Validation("background spawn requires a background prepare".to_owned())
        })?;
        let bash::PreparedBash {
            mut resolved,
            cwd,
            args,
            environment,
            deadline,
            request_timeout_ms,
            ..
        } = prepared.inner;
        if Instant::now() >= deadline {
            return Err(ProcessError::TimeoutBeforeSpawn {
                timeout_ms: request_timeout_ms,
            });
        }
        let args = if let Some(argv) = wrapped_argv {
            let command = argv.first().ok_or_else(|| {
                ProcessError::Validation(
                    "wrapped argv must contain at least the executable".to_owned(),
                )
            })?;
            resolved = ResolvedProgram {
                absolute: PathBuf::from(command),
                executable: PathBuf::from(command),
                launcher: launcher_for(std::path::Path::new(command))?,
            };
            argv[1..].to_vec()
        } else {
            args
        };
        let plan = ExecPlan {
            resolved: &resolved,
            cwd: &cwd,
            args: &args,
            environment: &environment,
            stdin: None,
            streams: Streams::Merged,
            timeout: Duration::ZERO,
            capture_page_bytes: context.output_budget.page_bytes(),
        };
        let (mut tree, reader) =
            crate::platform::process::spawn_detached_capture(&plan, &environment)?;
        let spawned_at = Instant::now();
        if context.cancellation.is_cancelled() || self.resources.shutdown_token().is_cancelled() {
            tree.terminate_and_wait(Instant::now() + crate::tools::exec::spawn::CLEANUP_DEADLINE)?;
            return Err(ProcessError::Cancelled);
        }
        Ok((tree, reader, background_timeout, spawned_at))
    }

    pub fn verify_bash(&self) -> Result<(), ProcessError> {
        self.bash_runtime().map(|_| ())
    }

    pub fn bash_runtime(&self) -> Result<(PathBuf, String), ProcessError> {
        self.bash_locator
            .resolve(&self.resources.shutdown_token())
            .map(|runtime| (runtime.executable.clone(), runtime.locale.clone()))
            .map_err(|error| match error {
                LocateError::Cancelled => ProcessError::Cancelled,
                LocateError::TimedOut => ProcessError::TimeoutBeforeSpawn {
                    timeout_ms: crate::tools::exec::DEFAULT_TIMEOUT_MS,
                },
                LocateError::Unavailable(message) => ProcessError::Unavailable(message.to_string()),
            })
    }

    async fn acquire_process(
        &self,
        deadline: std::time::Instant,
        timeout_ms: u64,
        memory_bytes: usize,
        cancellation: &CancellationToken,
    ) -> Result<(OwnedSemaphorePermit, OwnedSemaphorePermit), ProcessError> {
        let queued = Instant::now();
        self.ensure_process_active(cancellation)?;
        if Instant::now() >= deadline {
            return Err(ProcessError::TimeoutBeforeSpawn { timeout_ms });
        }
        let process = self.resources.try_admit_process().ok_or_else(|| {
            if cancellation.is_cancelled() || self.resources.shutdown_token().is_cancelled() {
                ProcessError::Cancelled
            } else {
                ProcessError::ResourceBusy("foreground process capacity is full".to_owned())
            }
        })?;
        let memory = tokio::time::timeout_at(
            tokio::time::Instant::from_std(deadline),
            self.resources.reserve_memory(memory_bytes, cancellation),
        )
        .await
        .map_err(|_| ProcessError::TimeoutBeforeSpawn { timeout_ms })?
        .map_err(|_| ProcessError::Cancelled)?;
        trace_capacity_acquired(queued);
        Ok((process, memory))
    }

    fn ensure_process_active(&self, cancellation: &CancellationToken) -> Result<(), ProcessError> {
        if cancellation.is_cancelled() || self.resources.shutdown_token().is_cancelled() {
            Err(ProcessError::Cancelled)
        } else {
            Ok(())
        }
    }

    fn apply_process_environment(&self, environment: &mut EnvironmentPlan) {
        if let Some(base) = &self.process_environment {
            environment.base = Some(base.entries.as_ref().clone());
        }
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

fn trace_capacity_acquired(queued: Instant) {
    tracing::info!(
        target: "agentshim",
        event = "capacity_acquired",
        phase = "queue",
        queue_ms = u64::try_from(queued.elapsed().as_millis()).unwrap_or(u64::MAX)
    );
}

fn prepared_argv(resolved: &ResolvedProgram, args: &[String]) -> Vec<String> {
    let mut command_argv = Vec::with_capacity(args.len().saturating_add(1));
    command_argv.push(resolved.executable.to_string_lossy().into_owned());
    command_argv.extend(args.iter().cloned());
    command_argv
}

fn cancelled_or_read_busy(
    request: &CancellationToken,
    resources: &RuntimeResources,
) -> read::ReadError {
    if request.is_cancelled() || resources.shutdown_token().is_cancelled() {
        read::ReadError::Cancelled
    } else {
        read::ReadError::ResourceBusy {
            resource: "read_only",
            retry_after: None,
        }
    }
}

fn cancelled_or_glob_busy(
    request: &CancellationToken,
    resources: &RuntimeResources,
) -> glob::GlobError {
    if request.is_cancelled() || resources.shutdown_token().is_cancelled() {
        glob::GlobError::Cancelled
    } else {
        glob::GlobError::ResourceBusy("read_only")
    }
}

fn cancelled_or_grep_busy(
    request: &CancellationToken,
    resources: &RuntimeResources,
) -> grep::GrepError {
    if request.is_cancelled() || resources.shutdown_token().is_cancelled() {
        grep::GrepError::Cancelled
    } else {
        grep::GrepError::ResourceBusy("read_only")
    }
}

fn cancelled_or_grep_concurrency_busy(
    request: &CancellationToken,
    resources: &RuntimeResources,
) -> grep::GrepError {
    if request.is_cancelled() || resources.shutdown_token().is_cancelled() {
        grep::GrepError::Cancelled
    } else {
        grep::GrepError::ResourceBusy("grep_concurrency")
    }
}

fn normalize_read_cancellation(
    error: read::ReadError,
    cancellation: &CancellationToken,
) -> read::ReadError {
    let cancellation_error = match &error {
        read::ReadError::Output(crate::output::OutputError::Cancelled)
        | read::ReadError::Cancelled => true,
        read::ReadError::Pdf(error) => {
            error.kind() == agentshim_pdf_read::PdfReadErrorKind::Cancelled
        }
        _ => false,
    };
    if cancellation.is_cancelled() && cancellation_error {
        read::ReadError::Cancelled
    } else {
        error
    }
}

fn normalize_glob_cancellation(
    error: glob::GlobError,
    cancellation: &CancellationToken,
) -> glob::GlobError {
    if cancellation.is_cancelled()
        && matches!(
            &error,
            glob::GlobError::Traversal(crate::traversal::TraversalError::Cancelled)
                | glob::GlobError::Output(crate::output::OutputError::Cancelled)
                | glob::GlobError::Cancelled
        )
    {
        glob::GlobError::Cancelled
    } else {
        error
    }
}

fn normalize_grep_cancellation(
    error: grep::GrepError,
    cancellation: &CancellationToken,
) -> grep::GrepError {
    if cancellation.is_cancelled()
        && matches!(
            &error,
            grep::GrepError::Traversal(crate::traversal::TraversalError::Cancelled)
                | grep::GrepError::Output(crate::output::OutputError::Cancelled)
                | grep::GrepError::Cancelled
        )
    {
        grep::GrepError::Cancelled
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
