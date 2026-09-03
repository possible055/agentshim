use std::{ffi::OsString, path::PathBuf, sync::Arc, time::Duration, time::Instant};

use tokio::sync::OwnedSemaphorePermit;

use tokio_util::sync::CancellationToken;

use super::{OperationContext, ToolEngine, trace_capacity_acquired};
use crate::tools::{
    ToolOutput,
    bash::{self, detached::DetachedAdmission, locate::LocateError},
    exec::{
        CaptureSink, ProcessError,
        resolve::{ResolvedProgram, launcher_for},
        spawn::{EnvironmentPlan, ExecPlan, Streams},
    },
    run_program,
};

#[derive(Clone, Debug)]
pub struct ProcessEnvironment {
    pub(super) entries: Arc<Vec<(String, String)>>,
    pub(super) bash_override: Option<OsString>,
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
    pub(crate) inner: run_program::PreparedRunProgram,
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
    pub(crate) inner: bash::PreparedBash,
    memory_charge: usize,
    background_timeout: Option<Duration>,
}

impl PreparedBash {
    #[must_use]
    pub fn argv(&self) -> Vec<String> {
        prepared_argv(&self.inner.resolved, &self.inner.args)
    }
}

fn prepared_argv(resolved: &ResolvedProgram, args: &[String]) -> Vec<String> {
    let mut command_argv = Vec::with_capacity(args.len().saturating_add(1));
    command_argv.push(resolved.executable.to_string_lossy().into_owned());
    command_argv.extend(args.iter().cloned());
    command_argv
}

impl ToolEngine {
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
        let PreparedRunProgram {
            inner,
            memory_charge,
        } = prepared;
        let windows_job_limits = self.resources.config().windows_job_limits;
        self.spawn_prepared_process(
            inner.deadline,
            inner.request_timeout_ms,
            memory_charge,
            &context,
            move |cancellation, output_budget| {
                crate::platform::process::with_windows_job_limits(windows_job_limits, || {
                    run_program::execute_prepared_run_program(
                        inner,
                        wrapped_argv.as_deref(),
                        cancellation,
                        output_budget,
                        capture_sink.as_ref(),
                    )
                })
            },
        )
        .await
    }

    pub async fn spawn_bash(
        &self,
        prepared: PreparedBash,
        wrapped_argv: Option<Vec<String>>,
        context: OperationContext,
        capture_sink: Option<Arc<dyn CaptureSink>>,
    ) -> Result<ToolOutput, ProcessError> {
        let PreparedBash {
            inner,
            memory_charge,
            ..
        } = prepared;
        let windows_job_limits = self.resources.config().windows_job_limits;
        self.spawn_prepared_process(
            inner.deadline,
            inner.request_timeout_ms,
            memory_charge,
            &context,
            move |cancellation, output_budget| {
                crate::platform::process::with_windows_job_limits(windows_job_limits, || {
                    bash::execute_prepared_bash(
                        inner,
                        wrapped_argv.as_deref(),
                        cancellation,
                        output_budget,
                        capture_sink.as_ref(),
                    )
                })
            },
        )
        .await
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
        let output_budget = Arc::clone(&context.output_budget);
        let windows_job_limits = self.resources.config().windows_job_limits;
        let span = tracing::Span::current();
        self.relayed_blocking(&context, ProcessError::Worker, move |cancellation| {
            span.in_scope(|| {
                let result =
                    crate::platform::process::with_windows_job_limits(windows_job_limits, || {
                        bash::execute_output_with_capture(
                            &root,
                            &locator,
                            Some(admission),
                            &request,
                            remaining,
                            cancellation,
                            background_timeout_max_ms,
                            output_budget.as_ref(),
                            None,
                        )
                    });
                if result.is_ok() {
                    on_commit();
                }
                drop(memory);
                result
            })
        })
        .await
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
        let (mut tree, reader) = crate::platform::process::with_windows_job_limits(
            self.resources.config().windows_job_limits,
            || crate::platform::process::spawn_detached_capture(&plan, &environment),
        )?;
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

    pub(super) async fn acquire_process(
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
        let foreground = self.resources.try_admit_foreground().ok_or_else(|| {
            if cancellation.is_cancelled() || self.resources.shutdown_token().is_cancelled() {
                ProcessError::Cancelled
            } else {
                ProcessError::ResourceBusy("foreground call capacity is full".to_owned())
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
        Ok((foreground, memory))
    }

    pub(super) fn ensure_process_active(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<(), ProcessError> {
        if cancellation.is_cancelled() || self.resources.shutdown_token().is_cancelled() {
            Err(ProcessError::Cancelled)
        } else {
            Ok(())
        }
    }

    pub(super) fn apply_process_environment(&self, environment: &mut EnvironmentPlan) {
        if let Some(base) = &self.process_environment {
            environment.base = Some(base.entries.as_ref().clone());
        }
    }
}
