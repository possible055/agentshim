use std::{fs, sync::Arc};

use rmcp::model::{CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock};
use serde::Deserialize;
use serde_json::json;

use super::{AgentShim, ToolAdmission, diagnostic_tool_error, shell_delegate, tool_error};
use crate::{
    output::MODEL_BYTE_LIMIT,
    server::response::{blocking_response_for_test, finalize_tool_response, parse_request},
};

mod admission;
mod catalog;
mod pdf_gate;

fn error_details(response: CallToolResponse) -> serde_json::Value {
    let CallToolResponse::Complete(result) = response else {
        panic!("tool error must be complete");
    };
    result
        .structured_content
        .as_ref()
        .expect("structured error")["error"]
        .clone()
}

fn response_text(response: &CallToolResponse) -> &str {
    let CallToolResponse::Complete(result) = response else {
        panic!("tool response must be complete");
    };
    let ContentBlock::Text(content) = &result.content[0] else {
        panic!("tool response must contain text");
    };
    &content.text
}

fn default_output_budget() -> crate::output::CallOutputBudget {
    crate::output::CallOutputBudget::standalone(MODEL_BYTE_LIMIT)
}

fn configured_server(
    root: &std::path::Path,
    profile: crate::ClientProfile,
    shelf: std::time::Duration,
    output_bytes: usize,
) -> AgentShim {
    let mut runtime = crate::runtime::RuntimeConfig::for_tests(1);
    runtime.output_bytes = output_bytes;
    runtime.tool_timeout_shelf = shelf;
    AgentShim::builder(root)
        .expect("builder")
        .client_profile(profile)
        .runtime_limits(runtime)
        .build()
        .expect("server")
}

fn read_path_call(server: &AgentShim, path: &str) -> CallToolResponse {
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("runtime")
        })
        .block_on(read_path_call_async(server, path))
}

async fn read_path_call_async(server: &AgentShim, path: &str) -> CallToolResponse {
    let arguments = json!({ "path": path })
        .as_object()
        .expect("arguments object")
        .clone();
    server
        .call_read(
            Some(arguments),
            &tokio_util::sync::CancellationToken::new(),
            &server.call_output_budget(),
        )
        .await
}

fn catalog_timeout(server: &AgentShim, field: &str) -> u64 {
    let tool =
        rmcp::ServerHandler::get_tool(server, "run_program").expect("run_program catalog entry");
    let value = serde_json::to_value(tool).expect("serialize tool");
    value["inputSchema"]["properties"]["timeout_ms"][field]
        .as_u64()
        .expect("timeout schema integer")
}

fn bash_catalog_timeout(server: &AgentShim, branch: usize, field: &str) -> u64 {
    let tool = rmcp::ServerHandler::get_tool(server, "bash").expect("bash catalog entry");
    let value = serde_json::to_value(tool).expect("serialize tool");
    value["inputSchema"]["oneOf"][branch]["properties"]["timeout_ms"][field]
        .as_u64()
        .expect("bash timeout schema integer")
}

async fn bash_with_timeout(server: &AgentShim, timeout_ms: u64) -> CallToolResponse {
    let arguments = json!({ "command": "true", "timeout_ms": timeout_ms })
        .as_object()
        .expect("bash arguments")
        .clone();
    let admission = ToolAdmission::ForegroundProcess;
    let budget = server.call_output_budget();
    server
        .call_bash_for_test(
            Some(arguments),
            &tokio_util::sync::CancellationToken::new(),
            admission,
            &budget,
        )
        .await
}

#[test]
fn tool_errors_are_bounded() {
    let budget = default_output_budget();
    let CallToolResponse::Complete(result) =
        tool_error(&budget, "validation", false, "界".repeat(40_000), None)
    else {
        panic!("tool error must be complete");
    };
    let ContentBlock::Text(content) = &result.content[0] else {
        panic!("tool error must contain text");
    };
    let text = &content.text;
    assert!(text.ends_with("...[truncated]"));
    assert!(text.len() <= MODEL_BYTE_LIMIT);

    let structured = result
        .structured_content
        .as_ref()
        .expect("tool error must contain structured content");
    assert_eq!(structured["error"]["code"], "validation");
    assert_eq!(structured["error"]["retryable"], false);
    assert_eq!(structured["error"]["message"], text.as_str());
    assert_eq!(result.is_error, Some(true));
    assert!(budget.tool_result_fits(text, Some(structured), true));
}

#[test]
fn tool_error_budget_counts_escaped_text_and_bounds_detail_captures() {
    let budget = default_output_budget();
    let details = json!({
        "stdout": { "text": "\\\"\u{1}".repeat(20_000), "total_bytes": 60_000 },
        "termination_outcome": "terminated"
    });
    let CallToolResponse::Complete(result) = tool_error(
        &budget,
        "resource_timeout",
        true,
        "\\\"\u{1}".repeat(20_000),
        Some(&details),
    ) else {
        panic!("tool error must be complete");
    };
    let ContentBlock::Text(content) = &result.content[0] else {
        panic!("tool error must contain text");
    };
    let structured = result
        .structured_content
        .as_ref()
        .expect("structured error");
    assert!(content.text.ends_with("...[truncated]"));
    assert_eq!(
        structured["error"]["details"]["termination_outcome"],
        "terminated"
    );
    assert!(budget.tool_result_fits(&content.text, Some(structured), true));
    let gate = crate::output::OutputTokenGate::load_shared().expect("token gate");
    assert!(matches!(
        gate.evaluate_result(&result, &tokio_util::sync::CancellationToken::new()),
        crate::output::GateDecision::FitsByBytes | crate::output::GateDecision::FitsExactly(_)
    ));
}

#[test]
fn successful_tool_responses_omit_structured_content() {
    let output = crate::tools::ToolOutput::with_child_nonzero("summary".to_owned(), true);
    let gate = crate::output::OutputTokenGate::load_shared().expect("token gate");
    let cancellation = tokio_util::sync::CancellationToken::new();
    let budget = default_output_budget();
    let CallToolResponse::Complete(result) =
        blocking_response_for_test::<crate::tools::exec::ProcessError>(
            "run_program",
            3,
            Ok(Ok(output)),
            &gate,
            &cancellation,
            &budget,
        )
    else {
        panic!("tool response must be complete");
    };

    let ContentBlock::Text(content) = &result.content[0] else {
        panic!("success response must contain text");
    };
    assert_eq!(content.text, "summary");
    assert_eq!(result.structured_content, None);
    assert_eq!(result.is_error, Some(false));

    let cursor_output = crate::tools::ToolOutput::new("cursor".to_owned())
        .with_structured(json!({ "transport": "private" }));
    let cursor_budget = default_output_budget();
    let CallToolResponse::Complete(cursor_result) =
        blocking_response_for_test::<crate::tools::exec::ProcessError>(
            "run_program",
            3,
            Ok(Ok(cursor_output)),
            &gate,
            &cancellation,
            &cursor_budget,
        )
    else {
        panic!("cursor response must be complete");
    };
    assert_eq!(cursor_result.structured_content, None);
}

#[test]
fn final_verifier_replaces_an_unbounded_model_payload() {
    let fixture = tempfile::tempdir().expect("fixture");
    let server = AgentShim::from_path(fixture.path()).expect("server");
    let budget = default_output_budget();
    let verified = blocking_response_for_test::<crate::tools::exec::ProcessError>(
        "run_program",
        3,
        Ok(Ok(crate::tools::ToolOutput::new(" x".repeat(10_000)))),
        server.output_token_gate.as_ref(),
        &tokio_util::sync::CancellationToken::new(),
        &budget,
    );
    let error = error_details(verified);

    assert_eq!(error["code"], "output_budget");
    assert_eq!(error["retryable"], false);
}

#[test]
fn exhausted_burst_returns_only_a_bounded_control_response() {
    let token_gate = crate::output::OutputTokenGate::load_shared().expect("token gate");
    let burst_gate = crate::output::BurstOutputGate::new(2_048);
    let spent = crate::output::CallOutputBudget::new(
        MODEL_BYTE_LIMIT,
        token_gate.clone(),
        burst_gate.begin_call(),
    );
    assert_eq!(spent.ceiling(), 2_048);
    spent.finish(2_048, false);
    let budget =
        crate::output::CallOutputBudget::new(MODEL_BYTE_LIMIT, token_gate, burst_gate.begin_call());
    let response = crate::server::response::finalize_tool_response(
        "read",
        &budget,
        Ok(CallToolResult::success(vec![ContentBlock::text(
            "unbounded source content".repeat(10_000),
        )])
        .into()),
        &tokio_util::sync::CancellationToken::new(),
    )
    .expect("bounded response");
    let details = error_details(response);
    assert_eq!(details["code"], "output_budget");
    assert_eq!(details["retryable"], true);
    assert_eq!(details["details"]["reason"], "burst_limit");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_instances_keep_timeout_catalog_dispatch_and_output_limits_isolated() {
    let fixture = tempfile::tempdir().expect("fixture");
    fs::write(
        fixture.path().join("large.txt"),
        format!("{}\n", "x".repeat(90)).repeat(600),
    )
    .expect("large fixture");

    let cursor_shelf = std::time::Duration::from_secs(30);
    let codex_shelf = std::time::Duration::from_secs(60);
    let cursor = configured_server(
        fixture.path(),
        crate::ClientProfile::Cursor,
        cursor_shelf,
        crate::output::MIN_OUTPUT_BYTES,
    );
    let codex = configured_server(
        fixture.path(),
        crate::ClientProfile::Codex,
        codex_shelf,
        48_000,
    );
    let cursor_max = agentshim_core::tools::exec::max_timeout_ms_from_shelf(cursor_shelf);
    let codex_max = agentshim_core::tools::exec::max_timeout_ms_from_shelf(codex_shelf);
    assert_eq!(catalog_timeout(&cursor, "maximum"), cursor_max);
    assert_eq!(catalog_timeout(&codex, "maximum"), codex_max);
    assert_eq!(bash_catalog_timeout(&cursor, 0, "maximum"), cursor_max);
    assert_eq!(bash_catalog_timeout(&codex, 0, "maximum"), codex_max);
    assert_eq!(
        bash_catalog_timeout(&codex, 1, "maximum"),
        1_800_000,
        "background policy must not subtract foreground cleanup/protocol slack",
    );
    assert_eq!(cursor.client_profile(), crate::ClientProfile::Cursor);
    assert_eq!(codex.client_profile(), crate::ClientProfile::Codex);

    let cursor_read = read_path_call_async(&cursor, "large.txt").await;
    let codex_read = read_path_call_async(&codex, "large.txt").await;
    assert!(
        response_text(&cursor_read).len() < response_text(&codex_read).len(),
        "the narrow instance did not apply its own output ceiling"
    );
    assert!(response_text(&cursor_read).len() <= crate::output::MIN_OUTPUT_BYTES);
    assert!(response_text(&codex_read).len() <= 48_000);

    let timeout_ms = cursor_max + 1;
    assert!(timeout_ms < codex_max);
    let cursor_response = bash_with_timeout(&cursor, timeout_ms).await;
    assert_eq!(error_details(cursor_response)["code"], "validation");

    if crate::bash_report().is_ok() {
        let CallToolResponse::Complete(codex_result) = bash_with_timeout(&codex, timeout_ms).await
        else {
            panic!("codex response must be complete");
        };
        assert_ne!(
            codex_result.is_error,
            Some(true),
            "the wider instance rejected a timeout its sibling accepts"
        );
    }

    let mut invalid_runtime = crate::runtime::RuntimeConfig::for_tests(1);
    invalid_runtime.output_bytes = crate::output::MIN_OUTPUT_BYTES - 1;
    let invalid = AgentShim::builder(fixture.path())
        .expect("invalid builder")
        .runtime_limits(invalid_runtime)
        .build();
    assert!(matches!(
        invalid,
        Err(error) if error.kind() == std::io::ErrorKind::InvalidInput
    ));
}

#[test]
fn unavailable_and_unsearchable_errors_map_to_io_with_explicit_retryability() {
    use crate::server::response::DiagnosticError;

    let budget = default_output_budget();
    let unavailable: crate::tools::exec::ProcessError =
        crate::tools::exec::ProcessError::Unavailable("no GNU bash".to_owned());
    let binary: crate::tools::grep::GrepError =
        crate::tools::grep::GrepError::Unsearchable(crate::output::SkipReason::Binary);
    let changed: crate::tools::grep::GrepError = crate::tools::grep::GrepError::Unsearchable(
        crate::output::SkipReason::ChangedWhileSearched,
    );
    for (error, retryable, message_needle) in [
        (&unavailable as &dyn DiagnosticError, false, None),
        (&binary, false, Some("binary")),
        (&changed, true, Some("changed")),
    ] {
        let CallToolResponse::Complete(result) = diagnostic_tool_error(&budget, error) else {
            panic!("tool error must be complete");
        };
        let structured = result
            .structured_content
            .as_ref()
            .expect("structured error");

        assert_eq!(structured["error"]["code"], "io");
        assert_eq!(structured["error"]["retryable"], retryable);
        assert_eq!(result.is_error, Some(true));
        if let Some(needle) = message_needle {
            assert!(
                structured["error"]["message"]
                    .as_str()
                    .expect("message")
                    .contains(needle)
            );
        }
    }
}
