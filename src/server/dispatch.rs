use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use rmcp::{
    ErrorData as McpError, RoleServer,
    model::{CallToolRequestParams, CallToolResponse, ErrorCode, JsonObject},
    service::RequestContext,
};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::tools::{
    bash::{
        BashRequest, BashToolRequest,
        detached::{DetachedAdmission, StopCause, StopStart},
        status::BashStatusRequest,
    },
    exec::ProcessError,
    run_program::ProcessRequest,
};

use super::{
    response::{
        blocking_response, cancellation_class, classified_tool_error, diagnostic_tool_error,
        duration_ms, parse_request, requests_detach, resource_busy, resource_busy_with_message,
    },
    service::AgentShim,
};

pub(super) enum ToolAdmission {
    ReadOnlyFacade,
    AuxiliaryReadOnly,
    ForegroundProcess,
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
    pub(super) async fn call_read(
        &self,
        arguments: Option<JsonObject>,
        request_cancellation: &CancellationToken,
        output_budget: &crate::output::CallOutputBudget,
    ) -> CallToolResponse {
        self.run_read_only_tool(
            "read",
            arguments,
            request_cancellation,
            output_budget,
            |engine, request, context| Box::pin(engine.read(request, context)),
        )
        .await
    }

    async fn call_glob(
        &self,
        arguments: Option<JsonObject>,
        request_cancellation: &CancellationToken,
        output_budget: &crate::output::CallOutputBudget,
    ) -> CallToolResponse {
        self.run_read_only_tool(
            "glob",
            arguments,
            request_cancellation,
            output_budget,
            |engine, request, context| Box::pin(engine.glob(request, context)),
        )
        .await
    }

    async fn call_grep(
        &self,
        arguments: Option<JsonObject>,
        request_cancellation: &CancellationToken,
        output_budget: &crate::output::CallOutputBudget,
    ) -> CallToolResponse {
        self.run_read_only_tool(
            "grep",
            arguments,
            request_cancellation,
            output_budget,
            |engine, request, context| Box::pin(engine.grep(request, context)),
        )
        .await
    }

    /// One pipeline for the three read-only tools: parse, run under the shared
    /// operation context, reclassify a shutdown-time cancellation, and render.
    async fn run_read_only_tool<R, E, F>(
        &self,
        tool: &'static str,
        arguments: Option<JsonObject>,
        request_cancellation: &CancellationToken,
        output_budget: &crate::output::CallOutputBudget,
        invoke: F,
    ) -> CallToolResponse
    where
        R: serde::de::DeserializeOwned,
        E: super::response::DiagnosticError,
        F: for<'engine> FnOnce(
            &'engine agentshim_core::ToolEngine,
            R,
            agentshim_core::OperationContext,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<crate::tools::ToolOutput, E>>
                    + Send
                    + 'engine,
            >,
        >,
    {
        let request: R = match parse_request(arguments, tool) {
            Ok(request) => request,
            Err(error) => return classified_tool_error(output_budget, "validation", error),
        };
        let running = Instant::now();
        let result = invoke(
            &self.tool_engine,
            request,
            agentshim_core::OperationContext::new(
                request_cancellation.clone(),
                Arc::new(output_budget.clone()),
            ),
        )
        .await;
        if result.is_err()
            && self.resources.shutdown_token().is_cancelled()
            && !request_cancellation.is_cancelled()
            && matches!(&result, Err(error) if error.error_class() == "client_cancellation")
        {
            return classified_tool_error(
                output_budget,
                "shutdown",
                format!("{tool} stopped by shutdown"),
            );
        }
        blocking_response(
            tool,
            duration_ms(running.elapsed()),
            Ok(result),
            self.output_token_gate.as_ref(),
            request_cancellation,
            output_budget,
        )
    }

    async fn call_process(
        &self,
        arguments: Option<JsonObject>,
        context: &RequestContext<RoleServer>,
        output_budget: &crate::output::CallOutputBudget,
    ) -> CallToolResponse {
        let request_cancellation = &context.ct;
        let process_request: ProcessRequest = match parse_request(arguments, "run_program") {
            Ok(request) => request,
            Err(error) => return classified_tool_error(output_budget, "validation", error),
        };
        let max_timeout_ms = self.max_timeout_ms();
        let operation = agentshim_core::OperationContext::new(
            request_cancellation.clone(),
            Arc::new(output_budget.clone()),
        );
        let prepared =
            match self
                .tool_engine
                .prepare_run_program(&process_request, max_timeout_ms, &operation)
            {
                Ok(prepared) => prepared,
                Err(error) => return diagnostic_tool_error(output_budget, &error),
            };
        let running = Instant::now();
        let result = self
            .tool_engine
            .spawn_run_program(prepared, None, operation, None)
            .await;
        blocking_response(
            "run_program",
            duration_ms(running.elapsed()),
            Ok(result),
            self.output_token_gate.as_ref(),
            request_cancellation,
            output_budget,
        )
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
            Err(error) => return classified_tool_error(output_budget, "validation", error),
        };
        match request {
            BashToolRequest::Run(request) => {
                self.call_bash_run(request, request_cancellation, admission, output_budget)
                    .await
            }
            BashToolRequest::Terminate(request) => {
                if let Err(error) = request.validate() {
                    return classified_tool_error(output_budget, "validation", error.to_string());
                }
                if !matches!(admission, ToolAdmission::DetachedControl) {
                    return classified_tool_error(
                        output_budget,
                        "validation",
                        "bash terminate request reached an incompatible admission class",
                    );
                }
                let started = Instant::now();
                let action = self
                    .detached
                    .begin_stop(&request.job_id, StopCause::Explicit);
                let render_budget = output_budget.clone();
                let result = match action {
                    Ok(StopStart::Immediate(snapshot)) => {
                        let rendered = crate::tools::bash::status::render_termination_with_budget(
                            &snapshot,
                            &CancellationToken::new(),
                            &render_budget,
                        );
                        Ok(rendered)
                    }
                    Ok(StopStart::Accepted(work)) => {
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
                blocking_response(
                    "bash",
                    duration_ms(started.elapsed()),
                    result,
                    self.output_token_gate.as_ref(),
                    request_cancellation,
                    output_budget,
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
            Err(error) => return classified_tool_error(output_budget, "validation", error),
        };
        match request {
            BashToolRequest::Run(request) => {
                self.call_bash_run(request, request_cancellation, admission, output_budget)
                    .await
            }
            BashToolRequest::Terminate(_) => classified_tool_error(
                output_budget,
                "validation",
                "test helper accepts bash run requests only",
            ),
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "admission, detached ownership, execution, and verification share one lifetime"
    )]
    async fn call_bash_run(
        &self,
        bash_request: BashRequest,
        request_cancellation: &CancellationToken,
        admission: ToolAdmission,
        output_budget: &crate::output::CallOutputBudget,
    ) -> CallToolResponse {
        let max_timeout_ms = self.max_timeout_ms();
        let timeout_ceiling_ms = if bash_request.detach {
            self.background_timeout_max_ms()
        } else {
            max_timeout_ms
        };
        if let Err(error) = bash_request.validate(timeout_ceiling_ms) {
            return classified_tool_error(output_budget, "validation", error.to_string());
        }
        // The parsed request, not the pre-admission peek, decides which resource this call
        // consumes. They can only disagree through a request that failed to parse as detached,
        // and that case must still be capped rather than trusted.
        let detached_admission = match (admission, bash_request.detach) {
            (ToolAdmission::ForegroundProcess, false) => None,
            (ToolAdmission::ForegroundProcess, true) => match self.detached.admit() {
                Ok(admission) => Some(admission),
                Err(ProcessError::ResourceBusy(message)) => {
                    return resource_busy_with_message(output_budget, "bash", "detached", message);
                }
                Err(error) => return diagnostic_tool_error(output_budget, &error),
            },
            (ToolAdmission::Detached(admission), true) => Some(admission),
            (ToolAdmission::Detached(admission), false) => {
                drop(admission);
                None
            }
            _ => unreachable!("bash is admitted as a foreground process or a detached call"),
        };
        let operation = agentshim_core::OperationContext::new(
            request_cancellation.clone(),
            Arc::new(output_budget.clone()),
        );
        let detached_response = bash_request.detach;
        let running = Instant::now();
        let result = if let Some(admission) = detached_admission {
            let job_id = admission.job_id().to_owned();
            let detached = self.detached.clone();
            let shutdown = self.resources.shutdown_token();
            self.tool_engine
                .run_detached_bash(
                    bash_request,
                    admission,
                    self.background_timeout_max_ms(),
                    max_timeout_ms,
                    move || Self::arm_detached_deadline(detached, shutdown, &job_id),
                    operation,
                )
                .await
        } else {
            let prepared =
                match self
                    .tool_engine
                    .prepare_bash(&bash_request, max_timeout_ms, &operation)
                {
                    Ok(prepared) => prepared,
                    Err(error) => return diagnostic_tool_error(output_budget, &error),
                };
            self.tool_engine
                .spawn_bash(prepared, None, operation, None)
                .await
        };
        let committed_response_cancellation = CancellationToken::new();
        blocking_response(
            "bash",
            duration_ms(running.elapsed()),
            Ok(result),
            self.output_token_gate.as_ref(),
            if detached_response {
                &committed_response_cancellation
            } else {
                request_cancellation
            },
            output_budget,
        )
    }

    fn arm_detached_deadline(
        detached: crate::tools::bash::detached::DetachedTrees,
        shutdown: CancellationToken,
        job_id: &str,
    ) {
        let Some(registration) = detached.deadline_registration(job_id) else {
            return;
        };
        tokio::spawn(async move {
            std::mem::drop(tokio::task::spawn_blocking(move || {
                const POLL: std::time::Duration = std::time::Duration::from_millis(50);
                let cause = loop {
                    if registration.finished().is_cancelled() || shutdown.is_cancelled() {
                        return;
                    }
                    match registration.log_quota_exceeded() {
                        Ok(true) => break StopCause::LogQuota,
                        Ok(false) => {}
                        Err(error) => {
                            tracing::error!(target: "agentshim", event = "detached_log_quota_monitor", phase = "lifecycle", outcome = "uncertain", error_class = "io", io_kind = ?error.kind());
                            break StopCause::LogQuotaMonitor;
                        }
                    }
                    if std::time::Instant::now() >= registration.deadline() {
                        break StopCause::Timeout;
                    }
                    std::thread::sleep(
                        registration
                            .deadline()
                            .saturating_duration_since(std::time::Instant::now())
                            .min(POLL),
                    );
                };
                if let Ok(StopStart::Accepted(work)) =
                    detached.begin_stop(registration.job_id(), cause)
                {
                    work.run();
                }
            }));
        });
    }

    async fn call_bash_status(
        &self,
        arguments: Option<JsonObject>,
        request_cancellation: &CancellationToken,
        output_budget: &crate::output::CallOutputBudget,
    ) -> CallToolResponse {
        let request: BashStatusRequest = match parse_request(arguments, "bash_status") {
            Ok(request) => request,
            Err(error) => return classified_tool_error(output_budget, "validation", error),
        };
        if let Err(error) = request.validate() {
            return classified_tool_error(output_budget, "validation", error.to_string());
        }
        let memory_charge = request.memory_charge();
        let detached = self.detached.clone();
        let execute_budget = output_budget.clone();
        let started = Instant::now();
        let result = self
            .tool_engine
            .auxiliary_read_only(
                memory_charge,
                request_cancellation.clone(),
                move |cancellation| {
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
                    crate::tools::bash::status::render_with_budget(
                        &snapshot,
                        request.max_bytes.unwrap_or(request.tail_bytes),
                        &cancellation,
                        &execute_budget,
                    )
                },
            )
            .await;
        let result = match result {
            Ok(result) => result,
            Err(agentshim_core::AuxiliaryError::Busy) => {
                return resource_busy(output_budget, "bash_status", "read_only");
            }
            Err(agentshim_core::AuxiliaryError::Cancelled) => {
                return classified_tool_error(
                    output_budget,
                    cancellation_class(request_cancellation, &self.resources.shutdown_token()),
                    "bash_status cancelled while waiting for runtime capacity",
                );
            }
            Err(agentshim_core::AuxiliaryError::Worker(error)) => {
                return classified_tool_error(output_budget, "worker_panic", error);
            }
        };
        blocking_response(
            "bash_status",
            duration_ms(started.elapsed()),
            Ok(result),
            self.output_token_gate.as_ref(),
            request_cancellation,
            output_budget,
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
            ("read", ToolAdmission::ReadOnlyFacade) => Ok(self
                .call_read(request.arguments, &context.ct, output_budget)
                .await),
            ("glob", ToolAdmission::ReadOnlyFacade) => Ok(self
                .call_glob(request.arguments, &context.ct, output_budget)
                .await),
            ("grep", ToolAdmission::ReadOnlyFacade) => Ok(self
                .call_grep(request.arguments, &context.ct, output_budget)
                .await),
            ("run_program", ToolAdmission::ForegroundProcess) => Ok(self
                .call_process(request.arguments, context, output_budget)
                .await),
            (
                "bash",
                admission @ (ToolAdmission::ForegroundProcess
                | ToolAdmission::Detached(_)
                | ToolAdmission::DetachedControl),
            ) => Ok(self
                .call_bash(request.arguments, context, admission, output_budget)
                .await),
            ("bash_status", ToolAdmission::AuxiliaryReadOnly) => Ok(self
                .call_bash_status(request.arguments, &context.ct, output_budget)
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
    ) -> Result<ToolAdmission, ProcessError> {
        match request.name.as_ref() {
            "read" | "glob" | "grep" => Ok(ToolAdmission::ReadOnlyFacade),
            "bash_status" => Ok(ToolAdmission::AuxiliaryReadOnly),
            "bash" if requests_bash_terminate(request.arguments.as_ref()) => {
                Ok(ToolAdmission::DetachedControl)
            }
            "bash" if requests_detach(request.arguments.as_ref()) => {
                self.detached.admit().map(ToolAdmission::Detached)
            }
            "run_program" | "bash" => Ok(ToolAdmission::ForegroundProcess),
            _ => Ok(ToolAdmission::None),
        }
    }
}
