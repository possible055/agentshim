use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use rmcp::{
    ErrorData as McpError, RoleServer,
    model::{CallToolRequestParams, CallToolResponse, ErrorCode, JsonObject},
    service::RequestContext,
};
use serde_json::Value;
use tokio::sync::OwnedSemaphorePermit;
use tokio_util::sync::CancellationToken;

use crate::tools::{
    bash::{
        BashRequest, BashToolRequest,
        detached::{DetachedAdmission, TerminateStart},
        status::BashStatusRequest,
    },
    exec::ProcessError,
    run_program::ProcessRequest,
};

use super::{
    response::{
        PdfAdmission, blocking_response_for_profile, cancellation_class, classified_tool_error,
        diagnostic_tool_error, duration_ms, parse_request, pdf_busy, pdf_timeout, queue_timeout,
        relayed_cancellation, requests_detach, resource_busy, resource_busy_with_message,
    },
    service::AgentShim,
};

pub(super) enum ToolAdmission {
    ReadOnly(OwnedSemaphorePermit),
    Process(OwnedSemaphorePermit),
    /// A detached `bash` call holds no foreground permit. Its capacity is the detached roster,
    /// which limits living process trees rather than threads and output memory, and the two
    /// must not be able to starve each other.
    Detached(DetachedAdmission),
    DetachedControl,
    None,
}

fn requests_bash_terminate(arguments: Option<&JsonObject>) -> bool {
    arguments
        .and_then(|arguments| arguments.get("action"))
        .and_then(Value::as_str)
        == Some("terminate")
}

#[derive(Debug)]
pub(super) enum ToolAdmissionFailure {
    Capacity(&'static str),
    Process(ProcessError),
}

fn first_shell_token(command: &str) -> Option<&str> {
    let command = command.trim_start();
    let first = command.as_bytes().first().copied()?;
    if first == b'\'' || first == b'"' {
        let end = command[1..].find(char::from(first))? + 1;
        return Some(&command[1..end]);
    }
    command.split_whitespace().next()
}

pub(super) fn shell_delegate(request: &CallToolRequestParams) -> &'static str {
    if request.name.as_ref() != "bash" {
        return "none";
    }
    let Some(command) = request
        .arguments
        .as_ref()
        .and_then(|arguments| arguments.get("command"))
        .and_then(Value::as_str)
    else {
        return "none";
    };
    let Some(token) = first_shell_token(command) else {
        return "none";
    };
    let file_name = token
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(token)
        .to_ascii_lowercase();
    if file_name == "bash.exe" {
        return "wsl";
    }
    let stem = file_name
        .rsplit_once('.')
        .map_or(file_name.as_str(), |(stem, _)| stem);
    match stem {
        "pwsh" | "powershell" => "pwsh",
        "cmd" => "cmd",
        "wsl" => "wsl",
        "python" | "node" | "perl" | "ruby" => "other-interpreter",
        _ => "none",
    }
}

impl AgentShim {
    // Read retries intentionally keep capability and reservation lifetimes in one scope.
    #[allow(clippy::too_many_lines)]
    pub(super) async fn call_read(
        &self,
        arguments: Option<JsonObject>,
        request_cancellation: &CancellationToken,
        admission: OwnedSemaphorePermit,
        output_budget: &crate::output::CallOutputBudget,
    ) -> CallToolResponse {
        let queued = Instant::now();
        let read_request: crate::tools::read::ReadRequest = match parse_request(arguments, "read") {
            Ok(request) => request,
            Err(error) => return classified_tool_error("validation", error),
        };
        let worker = self.resources.acquire_worker(request_cancellation).await;
        let open_file = self.resources.acquire_open_file(request_cancellation).await;
        let permits = match (worker, open_file) {
            (Ok(worker), Ok(open_file)) => (admission, worker, open_file),
            _ => {
                return classified_tool_error(
                    cancellation_class(request_cancellation, &self.resources.shutdown_token()),
                    "read cancelled while waiting for runtime capacity",
                );
            }
        };
        tracing::info!(target: "agentshim", event = "capacity_acquired", phase = "queue", queue_ms = duration_ms(queued.elapsed()));
        let access = self.file_access.clone();
        let (cancellation, cancellation_relay) =
            relayed_cancellation(request_cancellation, self.resources.shutdown_token());
        let span = tracing::Span::current();
        let running = Instant::now();
        let budgets = crate::tools::read::PdfMemoryBudgets::from_config(&self.resources.config());
        let timed_out: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
        let mut pdf_admission: Option<PdfAdmission> = None;
        let mut deadline = None;
        let mut result = None;
        for attempt_index in 0..2 {
            let prepare_access = access.clone();
            let prepare_request = read_request.clone();
            let prepare_cancellation = cancellation.clone();
            let prepare_span = span.clone();
            let prepared = tokio::task::spawn_blocking(move || {
                prepare_span.in_scope(|| {
                    crate::tools::read::prepare(
                        &prepare_access,
                        &prepare_request,
                        &prepare_cancellation,
                        budgets,
                    )
                })
            })
            .await;
            let prepared = match prepared {
                Ok(Ok(prepared)) => prepared,
                Ok(Err(crate::tools::read::ReadError::Io(error)))
                    if attempt_index > 0 && error.kind() == io::ErrorKind::NotFound =>
                {
                    result = Some(Ok(Err(crate::tools::read::ReadError::Changed)));
                    break;
                }
                Ok(Err(error)) => {
                    result = Some(Ok(Err(error)));
                    break;
                }
                Err(error) => {
                    result = Some(Err(error));
                    break;
                }
            };
            // A PDF holds its gate and reservation for the whole retry loop. Releasing
            // between attempts would let another call take the gate, so a caller who hit
            // `file_changed` once would come back with `resource_busy` instead — two
            // unrelated failures reported as one.
            let mut text_memory = None;
            if prepared.pdf_mode().is_some() {
                if pdf_admission.is_none() {
                    match self
                        .acquire_pdf_admission(&prepared, request_cancellation)
                        .await
                    {
                        Ok(acquired) => pdf_admission = Some(acquired),
                        Err(busy) => {
                            cancellation_relay.abort();
                            drop(permits);
                            return busy;
                        }
                    }
                }
                deadline = prepared.runtime_limit();
            } else {
                let Ok(memory) = self
                    .resources
                    .reserve_memory(prepared.memory_charge(), request_cancellation)
                    .await
                else {
                    cancellation_relay.abort();
                    drop(permits);
                    return classified_tool_error(
                        cancellation_class(request_cancellation, &self.resources.shutdown_token()),
                        "read cancelled while waiting for runtime capacity",
                    );
                };
                text_memory = Some(memory);
            }
            let execute_access = access.clone();
            let execute_request = read_request.clone();
            let execute_cancellation = cancellation.clone();
            let execute_output_budget = output_budget.clone();
            let execute_span = span.clone();
            let started = Instant::now();
            let timer = deadline.map(|limit| {
                let token = cancellation.clone();
                let expired = Arc::clone(&timed_out);
                tokio::spawn(async move {
                    tokio::time::sleep(limit).await;
                    // `spawn_blocking` cannot be preempted, so the ceiling is enforced by
                    // cancelling at the same checkpoints the client cancellation uses. The
                    // real stop latency is therefore checkpoint density, not this timer.
                    expired.store(true, Ordering::SeqCst);
                    token.cancel();
                })
            });
            let executed = tokio::task::spawn_blocking(move || {
                execute_span.in_scope(|| {
                    crate::tools::read::execute_prepared_with_budget(
                        &execute_access,
                        &execute_request,
                        prepared,
                        &execute_cancellation,
                        &execute_output_budget,
                    )
                })
            })
            .await;
            if let Some(timer) = timer {
                timer.abort();
            }
            drop(text_memory);
            // The elapsed check is not redundant with the flag: the timer is a spawned
            // task, so under load it can be scheduled after the work already overran. The
            // ceiling is a property of how long the work took, not of when a task ran.
            let overran = deadline.is_some_and(|limit| started.elapsed() > limit);
            if timed_out.load(Ordering::SeqCst) || overran {
                let limit = deadline.unwrap_or_default();
                cancellation_relay.abort();
                drop(pdf_admission);
                drop(permits);
                return pdf_timeout(limit, started.elapsed());
            }
            match executed {
                Ok(Ok(crate::tools::read::Attempt::Stable(output))) => {
                    result = Some(Ok(Ok(output)));
                    break;
                }
                Ok(Ok(crate::tools::read::Attempt::Changed)) if attempt_index == 0 => {
                    tracing::warn!(target: "agentshim", event = "read_retry", phase = "execution", outcome = "degraded_success", reason = "file_changed");
                }
                Ok(Ok(crate::tools::read::Attempt::Changed)) => {
                    result = Some(Ok(Err(crate::tools::read::ReadError::Changed)));
                    break;
                }
                Ok(Err(error)) => {
                    result = Some(Ok(Err(error)));
                    break;
                }
                Err(error) => {
                    result = Some(Err(error));
                    break;
                }
            }
        }
        let result = result.expect("read attempt loop always produces a result");
        drop(permits);
        // The PDF reservation outlives the response construction on purpose. Base64 image
        // data is copied again into content blocks and again by the JSON serialiser, and
        // that is the single largest allocation on the whole path. Releasing the
        // reservation before it happens would leave the biggest number off the books.
        let response = blocking_response_for_profile(
            "read",
            duration_ms(running.elapsed()),
            result,
            self.output_token_gate.as_deref(),
            &cancellation,
            output_budget,
            self.client_profile(),
        );
        cancellation_relay.abort();
        drop(pdf_admission);
        response
    }

    /// Acquire the PDF gate and this mode's memory reservation.
    ///
    /// The gate allows a bounded wait; the reservation does not. A waiting reservation is
    /// exactly the head-of-line block this separation exists to remove — an ordinary text
    /// read must never queue behind a PDF's share of the pool.
    async fn acquire_pdf_admission(
        &self,
        prepared: &crate::tools::read::PreparedRead,
        request_cancellation: &CancellationToken,
    ) -> Result<PdfAdmission, CallToolResponse> {
        let Some(gate) = self.resources.acquire_pdf_gate(request_cancellation).await else {
            return Err(pdf_busy("pdf_concurrency"));
        };
        let Some(memory) = self.resources.try_reserve_memory(prepared.memory_charge()) else {
            return Err(pdf_busy("memory_budget"));
        };
        Ok(PdfAdmission {
            _gate: gate,
            _memory: memory,
        })
    }

    async fn call_glob(
        &self,
        arguments: Option<JsonObject>,
        request_cancellation: &CancellationToken,
        admission: OwnedSemaphorePermit,
        output_budget: &crate::output::CallOutputBudget,
    ) -> CallToolResponse {
        let queued = Instant::now();
        let glob_request = match parse_request(arguments, "glob") {
            Ok(request) => request,
            Err(error) => return classified_tool_error("validation", error),
        };
        let worker = self.resources.acquire_worker(request_cancellation).await;
        let memory = self
            .resources
            .reserve_memory(
                crate::tools::glob::memory_charge(&glob_request),
                request_cancellation,
            )
            .await;
        let permits = match (worker, memory) {
            (Ok(worker), Ok(memory)) => (admission, worker, memory),
            _ => {
                return classified_tool_error(
                    cancellation_class(request_cancellation, &self.resources.shutdown_token()),
                    "glob cancelled while waiting for runtime capacity",
                );
            }
        };
        tracing::info!(target: "agentshim", event = "capacity_acquired", phase = "queue", queue_ms = duration_ms(queued.elapsed()));
        let access = self.file_access.clone();
        let resources = self.resources.clone();
        let reservation = crate::runtime::MemoryReservation::from_initial(
            resources.clone(),
            permits.2,
            crate::tools::glob::memory_charge(&glob_request),
        );
        let permits = (permits.0, permits.1);
        let (cancellation, cancellation_relay) =
            relayed_cancellation(request_cancellation, self.resources.shutdown_token());
        let response_cancellation = cancellation.clone();
        let execute_output_budget = output_budget.clone();
        let span = tracing::Span::current();
        let running = Instant::now();
        let result = tokio::task::spawn_blocking(move || {
            span.in_scope(|| {
                let result = crate::tools::glob::execute_output_with_budget(
                    &access,
                    &glob_request,
                    &resources,
                    &cancellation,
                    reservation,
                    &execute_output_budget,
                );
                drop(permits);
                result
            })
        })
        .await;
        let response = blocking_response_for_profile(
            "glob",
            duration_ms(running.elapsed()),
            result,
            self.output_token_gate.as_deref(),
            &response_cancellation,
            output_budget,
            self.client_profile(),
        );
        cancellation_relay.abort();
        response
    }

    async fn call_grep(
        &self,
        arguments: Option<JsonObject>,
        request_cancellation: &CancellationToken,
        admission: OwnedSemaphorePermit,
        output_budget: &crate::output::CallOutputBudget,
    ) -> CallToolResponse {
        let queued = Instant::now();
        let grep_request: crate::tools::grep::GrepRequest = match parse_request(arguments, "grep") {
            Ok(request) => request,
            Err(error) => return classified_tool_error("validation", error),
        };
        let worker = self.resources.acquire_worker(request_cancellation).await;
        let open_file = self.resources.acquire_open_file(request_cancellation).await;
        let memory = self
            .resources
            .reserve_memory(
                crate::tools::grep::base_memory_charge(self.resources.config().grep_memory_bytes),
                request_cancellation,
            )
            .await;
        let permits = match (worker, open_file, memory) {
            (Ok(worker), Ok(open_file), Ok(memory)) => (admission, worker, open_file, memory),
            _ => {
                return classified_tool_error(
                    cancellation_class(request_cancellation, &self.resources.shutdown_token()),
                    "grep cancelled while waiting for runtime capacity",
                );
            }
        };
        tracing::info!(target: "agentshim", event = "capacity_acquired", phase = "queue", queue_ms = duration_ms(queued.elapsed()));
        let access = self.file_access.clone();
        let resources = self.resources.clone();
        let reservation = crate::runtime::MemoryReservation::from_initial(
            resources.clone(),
            permits.3,
            crate::tools::grep::base_memory_charge(resources.config().grep_memory_bytes),
        );
        let permits = (permits.0, permits.1, permits.2);
        let (cancellation, cancellation_relay) =
            relayed_cancellation(request_cancellation, self.resources.shutdown_token());
        let response_cancellation = cancellation.clone();
        let execute_output_budget = output_budget.clone();
        let span = tracing::Span::current();
        let running = Instant::now();
        let result = tokio::task::spawn_blocking(move || {
            span.in_scope(|| {
                let result = crate::tools::grep::execute_output_with_budget(
                    &access,
                    &grep_request,
                    &resources,
                    &cancellation,
                    reservation,
                    &execute_output_budget,
                );
                drop(permits);
                result
            })
        })
        .await;
        let response = blocking_response_for_profile(
            "grep",
            duration_ms(running.elapsed()),
            result,
            self.output_token_gate.as_deref(),
            &response_cancellation,
            output_budget,
            self.client_profile(),
        );
        cancellation_relay.abort();
        response
    }

    async fn call_process(
        &self,
        arguments: Option<JsonObject>,
        context: &RequestContext<RoleServer>,
        admission: OwnedSemaphorePermit,
        output_budget: &crate::output::CallOutputBudget,
    ) -> CallToolResponse {
        let request_cancellation = &context.ct;
        let process_request: ProcessRequest = match parse_request(arguments, "run_program") {
            Ok(request) => request,
            Err(error) => return classified_tool_error("validation", error),
        };
        if let Err(error) = process_request.validate(super::service::max_timeout_ms()) {
            return classified_tool_error("validation", error.to_string());
        }
        let timeout =
            Duration::from_millis(process_request.timeout_ms(super::service::max_timeout_ms()));
        let memory_charge = process_request.memory_charge();
        let queued = Instant::now();
        let permits = tokio::time::timeout(timeout, async {
            let memory = self
                .resources
                .reserve_memory(memory_charge, request_cancellation)
                .await?;
            Ok::<_, crate::runtime::AcquireError>((admission, memory))
        })
        .await;
        let permits = match permits {
            Ok(Ok(permits)) => permits,
            Ok(Err(_)) => {
                return classified_tool_error(
                    cancellation_class(request_cancellation, &self.resources.shutdown_token()),
                    "run_program cancelled while waiting for process capacity",
                );
            }
            Err(_) => {
                return queue_timeout(
                    "run_program",
                    process_request.timeout_ms(super::service::max_timeout_ms()),
                );
            }
        };
        tracing::info!(target: "agentshim", event = "capacity_acquired", phase = "queue", queue_ms = duration_ms(queued.elapsed()));
        let Some(remaining) = timeout.checked_sub(queued.elapsed()) else {
            return queue_timeout(
                "run_program",
                process_request.timeout_ms(super::service::max_timeout_ms()),
            );
        };
        let root = self.root.clone();
        let resolver = self.process_resolver.clone();
        let (cancellation, cancellation_relay) =
            relayed_cancellation(request_cancellation, self.resources.shutdown_token());
        let response_cancellation = cancellation.clone();
        let execute_output_budget = output_budget.clone();
        let span = tracing::Span::current();
        let running = Instant::now();
        let result = tokio::task::spawn_blocking(move || {
            span.in_scope(|| {
                let Some(remaining) = remaining.checked_sub(running.elapsed()) else {
                    drop(permits);
                    return Err(ProcessError::TimeoutBeforeSpawn {
                        timeout_ms: process_request.timeout_ms(super::service::max_timeout_ms()),
                    });
                };
                let result = crate::tools::run_program::execute_output_with_capture(
                    &root,
                    &resolver,
                    &process_request,
                    remaining,
                    &cancellation,
                    super::service::max_timeout_ms(),
                    &execute_output_budget,
                    None,
                );
                drop(permits);
                result
            })
        })
        .await;
        let response = blocking_response_for_profile(
            "run_program",
            duration_ms(running.elapsed()),
            result,
            self.output_token_gate.as_deref(),
            &response_cancellation,
            output_budget,
            self.client_profile(),
        );
        cancellation_relay.abort();
        response
    }

    pub(super) async fn call_bash(
        &self,
        arguments: Option<JsonObject>,
        context: &RequestContext<RoleServer>,
        admission: ToolAdmission,
        output_budget: &crate::output::CallOutputBudget,
    ) -> CallToolResponse {
        let request_cancellation = &context.ct;
        let request: BashToolRequest = match parse_request(arguments, "bash") {
            Ok(request) => request,
            Err(error) => return classified_tool_error("validation", error),
        };
        match request {
            BashToolRequest::Run(request) => {
                self.call_bash_run(request, request_cancellation, admission, output_budget)
                    .await
            }
            BashToolRequest::Terminate(request) => {
                if let Err(error) = request.validate() {
                    return classified_tool_error("validation", error.to_string());
                }
                if !matches!(admission, ToolAdmission::DetachedControl) {
                    return classified_tool_error(
                        "validation",
                        "bash terminate request reached an incompatible admission class",
                    );
                }
                let started = Instant::now();
                let action = self.detached.begin_terminate(&request.job_id);
                let render_budget = output_budget.clone();
                let result = match action {
                    Ok(TerminateStart::Immediate(snapshot)) => {
                        let rendered = crate::tools::bash::status::render_termination_with_budget(
                            &snapshot,
                            &CancellationToken::new(),
                            &render_budget,
                        );
                        Ok(rendered)
                    }
                    Ok(TerminateStart::Accepted(work)) => {
                        tokio::task::spawn_blocking(move || {
                            let snapshot = work.run();
                            crate::tools::bash::status::render_termination_with_budget(
                                &snapshot,
                                &CancellationToken::new(),
                                &render_budget,
                            )
                        })
                        .await
                    }
                    Err(error) => Ok(Err(error)),
                };
                blocking_response_for_profile(
                    "bash",
                    duration_ms(started.elapsed()),
                    result,
                    self.output_token_gate.as_deref(),
                    request_cancellation,
                    output_budget,
                    self.client_profile(),
                )
            }
        }
    }

    #[cfg(test)]
    pub(super) async fn call_bash_for_test(
        &self,
        arguments: Option<JsonObject>,
        request_cancellation: &CancellationToken,
        admission: ToolAdmission,
        output_budget: &crate::output::CallOutputBudget,
    ) -> CallToolResponse {
        let request: BashToolRequest = match parse_request(arguments, "bash") {
            Ok(request) => request,
            Err(error) => return classified_tool_error("validation", error),
        };
        match request {
            BashToolRequest::Run(request) => {
                self.call_bash_run(request, request_cancellation, admission, output_budget)
                    .await
            }
            BashToolRequest::Terminate(_) => {
                classified_tool_error("validation", "test helper accepts bash run requests only")
            }
        }
    }

    // Admission, detached ownership, execution, and final verification share one lifetime.
    #[allow(clippy::too_many_lines)]
    async fn call_bash_run(
        &self,
        bash_request: BashRequest,
        request_cancellation: &CancellationToken,
        admission: ToolAdmission,
        output_budget: &crate::output::CallOutputBudget,
    ) -> CallToolResponse {
        if let Err(error) = bash_request.validate(super::service::max_timeout_ms()) {
            return classified_tool_error("validation", error.to_string());
        }
        // The parsed request, not the pre-admission peek, decides which resource this call
        // consumes. They can only disagree through a request that failed to parse as detached,
        // and that case must still be capped rather than trusted.
        let (foreground, detached_admission) = match (admission, bash_request.detach) {
            (ToolAdmission::Process(permit), false) => (Some(permit), None),
            (ToolAdmission::Process(permit), true) => {
                drop(permit);
                match self.detached.admit() {
                    Ok(admission) => (None, Some(admission)),
                    Err(ProcessError::ResourceBusy(message)) => {
                        return resource_busy_with_message("bash", "detached", message);
                    }
                    Err(error) => return diagnostic_tool_error(&error),
                }
            }
            (ToolAdmission::Detached(admission), true) => (None, Some(admission)),
            (ToolAdmission::Detached(admission), false) => {
                drop(admission);
                match self.resources.try_admit_process() {
                    Some(permit) => (Some(permit), None),
                    None => return resource_busy("bash", "process"),
                }
            }
            _ => unreachable!("bash is admitted as a process or a detached call"),
        };
        let timeout =
            Duration::from_millis(bash_request.timeout_ms(super::service::max_timeout_ms()));
        let queued = Instant::now();
        let memory_charge = bash_request.memory_charge();
        let permits = match tokio::time::timeout(timeout, async {
            let memory = self
                .resources
                .reserve_memory(memory_charge, request_cancellation)
                .await?;
            Ok::<_, crate::runtime::AcquireError>((foreground, memory))
        })
        .await
        {
            Ok(Ok(permits)) => Some(permits),
            Ok(Err(_)) => {
                return classified_tool_error(
                    cancellation_class(request_cancellation, &self.resources.shutdown_token()),
                    "bash cancelled while waiting for request memory",
                );
            }
            Err(_) => None,
        };
        let Some(permits) = permits else {
            return queue_timeout(
                "bash",
                bash_request.timeout_ms(super::service::max_timeout_ms()),
            );
        };
        tracing::info!(target: "agentshim", event = "capacity_acquired", phase = "queue", queue_ms = duration_ms(queued.elapsed()));
        let Some(remaining) = timeout.checked_sub(queued.elapsed()) else {
            return queue_timeout(
                "bash",
                bash_request.timeout_ms(super::service::max_timeout_ms()),
            );
        };
        let root = self.root.clone();
        let locator = self.bash_locator.clone();
        let (cancellation, cancellation_relay) =
            relayed_cancellation(request_cancellation, self.resources.shutdown_token());
        let response_cancellation = cancellation.clone();
        let detached_response = bash_request.detach;
        let execute_output_budget = output_budget.clone();
        let span = tracing::Span::current();
        let running = Instant::now();
        let result = tokio::task::spawn_blocking(move || {
            span.in_scope(|| {
                let remaining = if bash_request.detach {
                    remaining
                } else {
                    let Some(remaining) = remaining.checked_sub(running.elapsed()) else {
                        drop(permits);
                        return Err(ProcessError::TimeoutBeforeSpawn {
                            timeout_ms: bash_request.timeout_ms(super::service::max_timeout_ms()),
                        });
                    };
                    remaining
                };
                let result = crate::tools::bash::execute_output_with_capture(
                    &root,
                    &locator,
                    detached_admission,
                    &bash_request,
                    remaining,
                    &cancellation,
                    super::service::max_timeout_ms(),
                    &execute_output_budget,
                    None,
                );
                drop(permits);
                result
            })
        })
        .await;
        let committed_response_cancellation = CancellationToken::new();
        let response = blocking_response_for_profile(
            "bash",
            duration_ms(running.elapsed()),
            result,
            self.output_token_gate.as_deref(),
            if detached_response {
                &committed_response_cancellation
            } else {
                &response_cancellation
            },
            output_budget,
            self.client_profile(),
        );
        cancellation_relay.abort();
        response
    }

    async fn call_bash_status(
        &self,
        arguments: Option<JsonObject>,
        request_cancellation: &CancellationToken,
        admission: OwnedSemaphorePermit,
        output_budget: &crate::output::CallOutputBudget,
    ) -> CallToolResponse {
        let request: BashStatusRequest = match parse_request(arguments, "bash_status") {
            Ok(request) => request,
            Err(error) => return classified_tool_error("validation", error),
        };
        if let Err(error) = request.validate() {
            return classified_tool_error("validation", error.to_string());
        }
        let worker = self.resources.acquire_worker(request_cancellation).await;
        let memory = self
            .resources
            .reserve_memory(request.memory_charge(), request_cancellation)
            .await;
        let permits = match (worker, memory) {
            (Ok(worker), Ok(memory)) => (admission, worker, memory),
            _ => {
                return classified_tool_error(
                    cancellation_class(request_cancellation, &self.resources.shutdown_token()),
                    "bash_status cancelled while waiting for runtime capacity",
                );
            }
        };
        let detached = self.detached.clone();
        let cancellation = request_cancellation.clone();
        let execute_budget = output_budget.clone();
        let started = Instant::now();
        let result = tokio::task::spawn_blocking(move || {
            let read_snapshot = || {
                if let Some(cursor) = request.cursor {
                    detached.status_cursor(
                        &request.job_id,
                        cursor,
                        request.max_bytes.unwrap_or(request.tail_bytes),
                    )
                } else {
                    detached.status(&request.job_id, request.tail_bytes)
                }
            };
            let mut snapshot = read_snapshot()?;
            if request.wait_ms > 0
                && snapshot.log.bytes.is_empty()
                && matches!(
                    snapshot.state,
                    crate::tools::bash::status::JobState::Running
                        | crate::tools::bash::status::JobState::StatusUnknown
                        | crate::tools::bash::status::JobState::Finalizing
                        | crate::tools::bash::status::JobState::Terminating
                )
            {
                std::thread::sleep(Duration::from_millis(request.wait_ms));
                snapshot = read_snapshot()?;
            }
            let rendered = crate::tools::bash::status::render_with_budget(
                &snapshot,
                request.max_bytes.unwrap_or(request.tail_bytes),
                &cancellation,
                &execute_budget,
            );
            drop(permits);
            rendered
        })
        .await;
        blocking_response_for_profile(
            "bash_status",
            duration_ms(started.elapsed()),
            result,
            self.output_token_gate.as_deref(),
            request_cancellation,
            output_budget,
            self.client_profile(),
        )
    }
    pub(super) async fn dispatch_tool(
        &self,
        request: CallToolRequestParams,
        context: &RequestContext<RoleServer>,
        admission: ToolAdmission,
        output_budget: &crate::output::CallOutputBudget,
    ) -> Result<CallToolResponse, McpError> {
        match (request.name.as_ref(), admission) {
            ("read", ToolAdmission::ReadOnly(admission)) => Ok(self
                .call_read(request.arguments, &context.ct, admission, output_budget)
                .await),
            ("glob", ToolAdmission::ReadOnly(admission)) => Ok(self
                .call_glob(request.arguments, &context.ct, admission, output_budget)
                .await),
            ("grep", ToolAdmission::ReadOnly(admission)) => Ok(self
                .call_grep(request.arguments, &context.ct, admission, output_budget)
                .await),
            ("run_program", ToolAdmission::Process(admission)) => Ok(self
                .call_process(request.arguments, context, admission, output_budget)
                .await),
            (
                "bash",
                admission @ (ToolAdmission::Process(_)
                | ToolAdmission::Detached(_)
                | ToolAdmission::DetachedControl),
            ) => Ok(self
                .call_bash(request.arguments, context, admission, output_budget)
                .await),
            ("bash_status", ToolAdmission::ReadOnly(admission)) => Ok(self
                .call_bash_status(request.arguments, &context.ct, admission, output_budget)
                .await),
            (_, ToolAdmission::None) => {
                tracing::error!(target: "agentshim", event = "tool_unknown", phase = "request", error_class = "validation");
                Err(McpError::new(
                    ErrorCode::METHOD_NOT_FOUND,
                    format!("unknown tool: {}", request.name),
                    None,
                ))
            }
            _ => unreachable!("tool admission class must match the dispatched tool"),
        }
    }

    pub(super) fn try_admit_tool(
        &self,
        request: &CallToolRequestParams,
    ) -> Result<ToolAdmission, ToolAdmissionFailure> {
        match request.name.as_ref() {
            "read" | "glob" | "grep" | "bash_status" => self
                .resources
                .try_admit_read_only()
                .map(ToolAdmission::ReadOnly)
                .ok_or(ToolAdmissionFailure::Capacity("read_only")),
            "bash" if requests_bash_terminate(request.arguments.as_ref()) => {
                Ok(ToolAdmission::DetachedControl)
            }
            "bash" if requests_detach(request.arguments.as_ref()) => self
                .detached
                .admit()
                .map(ToolAdmission::Detached)
                .map_err(ToolAdmissionFailure::Process),
            "run_program" | "bash" => self
                .resources
                .try_admit_process()
                .map(ToolAdmission::Process)
                .ok_or(ToolAdmissionFailure::Capacity("process")),
            _ => Ok(ToolAdmission::None),
        }
    }
}
