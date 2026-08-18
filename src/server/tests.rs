use std::{fs, sync::Arc};

use rmcp::model::{CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock};
use serde::Deserialize;
use serde_json::json;

use super::{
    AgentShim, ToolAdmission, ToolAdmissionFailure, diagnostic_tool_error, shell_delegate,
    tool_error,
};
use crate::{
    output::MODEL_BYTE_LIMIT,
    server::response::{blocking_response_for_test, finalize_tool_response, parse_request},
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConstraintFixture {
    timeout_ceiling_ms: u64,
    cases: Vec<ConstraintCase>,
}

#[derive(Deserialize)]
struct ConstraintCase {
    id: String,
    tool: String,
    rule: String,
    field: String,
    args: serde_json::Value,
}

fn constraint_fixture() -> ConstraintFixture {
    serde_json::from_str(include_str!("../../evals/host-constraints.json"))
        .expect("shared constraint fixture")
}

fn pdf_fixture() -> Vec<u8> {
    let bodies: [&[u8]; 5] = [
        b"<< /Type /Catalog /Pages 2 0 R >>",
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources \
              << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>",
        b"<< /Length 47 >>\nstream\nBT /F1 18 Tf 20 150 Td (PDF gate probe) Tj ET\nendstream",
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>",
    ];
    let mut pdf = b"%PDF-1.7\n".to_vec();
    let mut offsets = vec![0_usize; bodies.len() + 1];
    for (index, body) in bodies.iter().enumerate() {
        let id = index + 1;
        offsets[id] = pdf.len();
        pdf.extend_from_slice(format!("{id} 0 obj\n").as_bytes());
        pdf.extend_from_slice(body);
        pdf.extend_from_slice(b"\nendobj\n");
    }
    let size = bodies.len() + 1;
    let xref = pdf.len();
    pdf.extend_from_slice(format!("xref\n0 {size}\n0000000000 65535 f \n").as_bytes());
    for offset in offsets.iter().skip(1) {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!("trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n").as_bytes(),
    );
    pdf.extend_from_slice(format!("{xref}\n%%EOF\n").as_bytes());
    pdf
}

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

#[test]
fn occupied_pdf_gate_rejects_pdf_without_blocking_text_reads() {
    let fixture = tempfile::tempdir().expect("fixture");
    fs::write(fixture.path().join("document.pdf"), pdf_fixture()).expect("pdf");
    fs::write(fixture.path().join("notes.txt"), "alpha\nbeta\n").expect("text");
    let server = AgentShim::from_path(fixture.path()).expect("server");
    let _occupied = server
        .tool_engine
        .try_acquire_pdf_gate_for_test()
        .expect("PDF gate fixture");

    let CallToolResponse::Complete(text) = read_path_call(&server, "notes.txt") else {
        panic!("text read response must be complete");
    };
    assert_eq!(text.is_error, Some(false));

    let error = error_details(read_path_call(&server, "document.pdf"));
    assert_eq!(error["code"], "resource_busy");
    assert_eq!(error["details"]["permit"], "pdf_concurrency");
    assert_eq!(
        error["details"]["retry_after_ms"],
        json!(u64::try_from(crate::runtime::PDF_GATE_WAIT.as_millis()).expect("bound"))
    );
}

fn read_path_call(server: &AgentShim, path: &str) -> CallToolResponse {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime")
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

fn catalog_timeout(server: &AgentShim, field: &str) -> u64 {
    let tool =
        rmcp::ServerHandler::get_tool(server, "run_program").expect("run_program catalog entry");
    let value = serde_json::to_value(tool).expect("serialize tool");
    value["inputSchema"]["properties"]["timeout_ms"][field]
        .as_u64()
        .expect("timeout schema integer")
}

fn core_rejects_constraint(case: &ConstraintCase, timeout_ceiling_ms: u64) -> bool {
    let arguments = case.args.as_object().cloned();
    match case.tool.as_str() {
        "read" => parse_request::<crate::tools::read::ReadRequest>(arguments, "read")
            .and_then(|request| request.validate().map_err(|error| error.to_string()))
            .is_err(),
        "grep" => parse_request::<crate::tools::grep::GrepRequest>(arguments, "grep")
            .and_then(|request| request.validate().map_err(|error| error.to_string()))
            .is_err(),
        "glob" => parse_request::<crate::tools::glob::GlobRequest>(arguments, "glob")
            .and_then(|request| request.validate().map_err(|error| error.to_string()))
            .is_err(),
        "run_program" => {
            parse_request::<crate::tools::run_program::ProcessRequest>(arguments, "run_program")
                .and_then(|request| {
                    request
                        .validate(timeout_ceiling_ms)
                        .map_err(|error| error.to_string())
                })
                .is_err()
        }
        other => panic!("unknown constraint fixture tool {other}"),
    }
}

fn string_array(value: &serde_json::Value) -> Vec<String> {
    value
        .as_array()
        .expect("string array")
        .iter()
        .map(|value| value.as_str().expect("string").to_owned())
        .collect()
}

#[test]
fn shared_constraints_match_mcp_catalog_and_core_validation() {
    let fixture = constraint_fixture();
    let root = tempfile::tempdir().expect("root");
    let server = AgentShim::from_path(root.path()).expect("server");

    for case in &fixture.cases {
        assert!(
            core_rejects_constraint(case, fixture.timeout_ceiling_ms),
            "core accepted shared invalid case {}",
            case.id
        );

        let tool = rmcp::ServerHandler::get_tool(&server, &case.tool)
            .unwrap_or_else(|| panic!("missing MCP catalog tool {}", case.tool));
        let value = serde_json::to_value(tool).expect("serialize tool");
        let schema = &value["inputSchema"];
        let property = &schema["properties"][&case.field];
        match case.rule.as_str() {
            "required" => assert!(
                string_array(&schema["required"]).contains(&case.field),
                "catalog required mismatch for {}",
                case.id
            ),
            "non_empty" => assert_eq!(
                property["minLength"],
                json!(1),
                "catalog non-empty mismatch for {}",
                case.id
            ),
            "range" | "timeout" => {
                let candidate = case.args[&case.field].as_u64().expect("range candidate");
                let below_minimum = property["minimum"]
                    .as_u64()
                    .is_some_and(|minimum| candidate < minimum);
                let above_maximum = property["maximum"]
                    .as_u64()
                    .is_some_and(|maximum| candidate > maximum);
                assert!(
                    below_minimum || above_maximum,
                    "catalog range mismatch for {}",
                    case.id
                );
            }
            "unknown" => {
                assert_eq!(schema["additionalProperties"], json!(false));
                assert!(schema["properties"].get(case.field.as_str()).is_none());
            }
            "cross_field" => {
                for field in case.args.as_object().expect("arguments").keys() {
                    assert!(
                        schema["properties"].get(field.as_str()).is_some(),
                        "catalog lost cross-field input {field} for {}",
                        case.id
                    );
                }
            }
            other => panic!("unknown constraint fixture rule {other}"),
        }
    }
}

#[test]
fn mcp_catalog_matches_host_divergence_snapshot() {
    let expected: serde_json::Value =
        serde_json::from_str(include_str!("../../evals/host-divergence.json"))
            .expect("host divergence fixture");
    let root = tempfile::tempdir().expect("root");
    let server = AgentShim::from_path(root.path()).expect("server");

    let bash = rmcp::ServerHandler::get_tool(&server, "bash").expect("bash");
    let bash = serde_json::to_value(bash).expect("serialize bash");
    let actual_variants = bash["inputSchema"]["oneOf"]
        .as_array()
        .expect("bash variants")
        .iter()
        .map(|variant| {
            let mut fields = variant["properties"]
                .as_object()
                .expect("bash properties")
                .keys()
                .cloned()
                .collect::<Vec<_>>();
            fields.sort();
            let mut required = string_array(&variant["required"]);
            required.sort();
            (fields, required)
        })
        .collect::<Vec<_>>();
    let expected_variants = expected["bash"]["mcp"]["variants"]
        .as_array()
        .expect("expected bash variants")
        .iter()
        .map(|variant| {
            let mut fields = string_array(&variant["fields"]);
            fields.sort();
            let mut required = string_array(&variant["required"]);
            required.sort();
            (fields, required)
        })
        .collect::<Vec<_>>();
    assert_eq!(actual_variants, expected_variants);

    let status = rmcp::ServerHandler::get_tool(&server, "bash_status").expect("bash_status");
    let status = serde_json::to_value(status).expect("serialize bash_status");
    let mut fields = status["inputSchema"]["properties"]
        .as_object()
        .expect("bash_status properties")
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    fields.sort();
    let mut expected_fields = string_array(&expected["bash_status"]["mcp"]["fields"]);
    expected_fields.sort();
    assert_eq!(fields, expected_fields);
    assert_eq!(
        string_array(&status["inputSchema"]["required"]),
        string_array(&expected["bash_status"]["mcp"]["required"])
    );
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

// The second attempt after `file_changed` must not have to re-take the gate. With a
// single slot, re-taking inside the same call would time out and surface as
// `resource_busy`, turning one retryable condition into an unrelated one.
#[test]
fn a_file_changed_retry_holds_the_pdf_gate_instead_of_re_taking_it() {
    let _serialised = crate::tools::read::global_read_state_guard();
    let fixture = tempfile::tempdir().expect("fixture");
    fs::write(fixture.path().join("document.pdf"), pdf_fixture()).expect("pdf");
    let server = AgentShim::from_path(fixture.path()).expect("server");

    let before = server.tool_engine.pdf_gate_acquisitions_for_test();
    crate::tools::read::FORCED_CHANGES.store(1, std::sync::atomic::Ordering::SeqCst);
    let response = read_path_call(&server, "document.pdf");
    crate::tools::read::FORCED_CHANGES.store(0, std::sync::atomic::Ordering::SeqCst);

    let CallToolResponse::Complete(result) = response else {
        panic!("read response must be complete");
    };
    assert_eq!(
        result.is_error,
        Some(false),
        "a forced retry must still succeed: {:?}",
        result.structured_content
    );
    assert_eq!(
        server.tool_engine.pdf_gate_acquisitions_for_test() - before,
        1,
        "the gate was taken more than once across the retry loop"
    );
    assert_eq!(server.tool_engine.available_pdf_slots_for_test(), 1);
}

// The ceiling is enforced by cancelling at the same checkpoints client cancellation
// uses, because `spawn_blocking` cannot be preempted. A cancellation the server
// initiated must surface as `resource_timeout`, not as the client's cancellation.
#[test]
fn a_mode_runtime_ceiling_reports_resource_timeout_and_frees_the_gate() {
    let _serialised = crate::tools::read::global_read_state_guard();
    let fixture = tempfile::tempdir().expect("fixture");
    fs::write(fixture.path().join("document.pdf"), pdf_fixture()).expect("pdf");
    let server = AgentShim::from_path(fixture.path()).expect("server");

    crate::tools::read::FORCED_PDF_RUNTIME_LIMIT.store(1, std::sync::atomic::Ordering::SeqCst);
    let response = read_path_call(&server, "document.pdf");
    crate::tools::read::FORCED_PDF_RUNTIME_LIMIT.store(0, std::sync::atomic::Ordering::SeqCst);

    let details = error_details(response);
    assert_eq!(details["code"], "resource_timeout");
    assert_eq!(details["retryable"], true);
    assert_eq!(details["details"]["work_stopped"], true);
    assert_eq!(details["details"]["partial_output"], false);
    assert_eq!(
        server.tool_engine.available_pdf_slots_for_test(),
        1,
        "a timed-out call must release the gate"
    );
    assert!(
        server.tool_engine.available_memory_bytes_for_test()
            == server.runtime_limits().memory_bytes,
        "a timed-out call must release its reservation"
    );
}

#[test]
fn every_pdf_read_path_returns_the_gate_and_its_reservation() {
    let _serialised = crate::tools::read::global_read_state_guard();
    let fixture = tempfile::tempdir().expect("fixture");
    fs::write(fixture.path().join("document.pdf"), pdf_fixture()).expect("pdf");
    fs::write(fixture.path().join("broken.pdf"), b"%PDF-1.7\nnot a pdf\n").expect("broken");
    let server = AgentShim::from_path(fixture.path()).expect("server");
    let free_memory = || {
        server.tool_engine.available_memory_bytes_for_test() == server.runtime_limits().memory_bytes
    };

    for path in ["document.pdf", "broken.pdf", "missing.pdf"] {
        let _ = read_path_call(&server, path);
        assert_eq!(
            server.tool_engine.available_pdf_slots_for_test(),
            1,
            "{path} leaked the PDF gate"
        );
        assert!(free_memory(), "{path} leaked its memory reservation");
    }
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
    assert!(
        crate::output::tool_result_encoded_len(text, Some(structured), true) <= MODEL_BYTE_LIMIT
    );
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

    let codex_response = bash_with_timeout(&codex, timeout_ms).await;
    let CallToolResponse::Complete(codex_result) = codex_response else {
        panic!("codex response must be complete");
    };
    if codex_result.is_error == Some(true) {
        assert_ne!(
            codex_result.structured_content.as_ref().expect("error")["error"]["code"],
            "validation"
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
fn unavailable_bash_response_is_io_and_not_retryable() {
    let budget = default_output_budget();
    let error = crate::tools::exec::ProcessError::Unavailable("no GNU bash".to_owned());
    let CallToolResponse::Complete(result) = diagnostic_tool_error(&budget, &error) else {
        panic!("tool error must be complete");
    };
    let structured = result
        .structured_content
        .as_ref()
        .expect("structured error");

    assert_eq!(structured["error"]["code"], "io");
    assert_eq!(structured["error"]["retryable"], false);
    assert_eq!(result.is_error, Some(true));
}

#[test]
fn unsearchable_binary_is_io_and_not_retryable() {
    let budget = default_output_budget();
    let error = crate::tools::grep::GrepError::Unsearchable(crate::output::SkipReason::Binary);
    let CallToolResponse::Complete(result) = diagnostic_tool_error(&budget, &error) else {
        panic!("tool error must be complete");
    };
    let structured = result
        .structured_content
        .as_ref()
        .expect("structured error");

    assert_eq!(structured["error"]["code"], "io");
    assert_eq!(structured["error"]["retryable"], false);
    assert!(
        structured["error"]["message"]
            .as_str()
            .expect("message")
            .contains("binary")
    );
}

#[test]
fn unsearchable_changed_is_io_and_retryable() {
    let budget = default_output_budget();
    let error = crate::tools::grep::GrepError::Unsearchable(
        crate::output::SkipReason::ChangedWhileSearched,
    );
    let CallToolResponse::Complete(result) = diagnostic_tool_error(&budget, &error) else {
        panic!("tool error must be complete");
    };
    let structured = result
        .structured_content
        .as_ref()
        .expect("structured error");

    assert_eq!(structured["error"]["code"], "io");
    assert_eq!(structured["error"]["retryable"], true);
    assert!(
        structured["error"]["message"]
            .as_str()
            .expect("message")
            .contains("changed")
    );
}

fn detached_request() -> CallToolRequestParams {
    serde_json::from_value(json!({
        "name": "bash",
        "arguments": {
            "command": "sleep 30",
            "detach": true,
            "log_path": "build.log"
        }
    }))
    .expect("call tool request")
}

fn bash_request(command: &str) -> CallToolRequestParams {
    serde_json::from_value(json!({
        "name": "bash",
        "arguments": { "command": command }
    }))
    .expect("bash request")
}

fn bash_terminate_request() -> CallToolRequestParams {
    serde_json::from_value(json!({
        "name": "bash",
        "arguments": {
            "action": "terminate",
            "job_id": format!("bash-{}", uuid::Uuid::new_v4())
        }
    }))
    .expect("bash terminate request")
}

fn bash_status_request() -> CallToolRequestParams {
    serde_json::from_value(json!({
        "name": "bash_status",
        "arguments": { "job_id": format!("bash-{}", uuid::Uuid::new_v4()) }
    }))
    .expect("bash status request")
}

#[test]
fn shell_delegate_classifies_only_the_first_token_file_stem() {
    for (command, expected) in [
        ("pwsh -NoProfile -File release.ps1", "pwsh"),
        (
            r#""C:\Program Files\PowerShell\7\powershell.exe" -Command x"#,
            "pwsh",
        ),
        ("cmd.exe /c ver", "cmd"),
        (r"C:\Windows\System32\wsl.exe --status", "wsl"),
        (r"C:\Windows\System32\bash.exe -lc true", "wsl"),
        ("python.exe -c pass", "other-interpreter"),
        ("node script.js", "other-interpreter"),
        ("git pwsh -Command Get-Process", "none"),
        ("bash -lc true", "none"),
    ] {
        assert_eq!(
            shell_delegate(&bash_request(command)),
            expected,
            "{command}"
        );
    }
}

#[test]
fn detached_admission_reserves_before_blocking_scheduling_and_fails_fast() {
    let fixture = tempfile::tempdir().expect("fixture");
    let mut runtime = crate::runtime::RuntimeConfig::for_tests(1);
    runtime.detached_calls = 1;
    let server = AgentShim::builder(fixture.path())
        .expect("builder")
        .runtime_limits(runtime)
        .build()
        .expect("server");
    let request = detached_request();
    let first = server
        .try_admit_tool(&request)
        .expect("first detached admission");

    assert_eq!(server.detached.reserved_count(), 1);
    assert!(matches!(
        server.try_admit_tool(&request),
        Err(ToolAdmissionFailure::Process(
            crate::tools::exec::ProcessError::ResourceBusy(_)
        ))
    ));
    drop(first);
    assert_eq!(server.detached.reserved_count(), 0);
}

#[test]
fn foreground_saturation_does_not_consume_detached_capacity() {
    let fixture = tempfile::tempdir().expect("fixture");
    let mut runtime = crate::runtime::RuntimeConfig::for_tests(1);
    runtime.process_calls = 1;
    runtime.detached_calls = 1;
    let server = AgentShim::builder(fixture.path())
        .expect("builder")
        .runtime_limits(runtime)
        .build()
        .expect("server");
    let foreground = server
        .resources
        .try_admit_process_for_test()
        .expect("foreground admission");

    assert!(server.resources.try_admit_process_for_test().is_none());
    let detached = server
        .try_admit_tool(&detached_request())
        .expect("detached admission remains independent");
    assert!(matches!(detached, ToolAdmission::Detached(_)));
    drop(foreground);
}

#[test]
fn detached_control_bypasses_process_and_detached_capacity() {
    let fixture = tempfile::tempdir().expect("fixture");
    let mut runtime = crate::runtime::RuntimeConfig::for_tests(1);
    runtime.process_calls = 1;
    runtime.detached_calls = 1;
    let server = AgentShim::builder(fixture.path())
        .expect("builder")
        .runtime_limits(runtime)
        .build()
        .expect("server");
    let _foreground = server
        .resources
        .try_admit_process_for_test()
        .expect("foreground admission");
    let _detached = server.detached.admit().expect("detached reservation");

    assert!(matches!(
        server
            .try_admit_tool(&bash_terminate_request())
            .expect("detached control"),
        ToolAdmission::DetachedControl
    ));
    assert!(matches!(
        server
            .try_admit_tool(&bash_status_request())
            .expect("status admission"),
        ToolAdmission::AuxiliaryReadOnly
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_after_detached_commit_preserves_the_job_id_response() {
    if crate::bash_report().is_err() {
        return;
    }
    let fixture = tempfile::tempdir().expect("fixture");
    let server = AgentShim::from_path(fixture.path()).expect("server");
    let request = detached_request();
    let admission = server.try_admit_tool(&request).expect("detached admission");
    let entered = Arc::new(std::sync::Barrier::new(2));
    let release = Arc::new(std::sync::Barrier::new(2));
    let hook_entered = Arc::clone(&entered);
    let hook_release = Arc::clone(&release);
    server.detached.set_after_commit_hook(move || {
        hook_entered.wait();
        hook_release.wait();
    });
    let cancellation = tokio_util::sync::CancellationToken::new();
    let worker_cancellation = cancellation.clone();
    let budget = default_output_budget();
    let worker_budget = budget.clone();
    let worker_server = server.clone();
    let worker = tokio::spawn(async move {
        worker_server
            .call_bash_for_test(
                request.arguments,
                &worker_cancellation,
                admission,
                &worker_budget,
            )
            .await
    });

    entered.wait();
    cancellation.cancel();
    release.wait();
    let response = worker.await.expect("detached worker");
    let response = finalize_tool_response("bash", &budget, Ok(response), &cancellation)
        .expect("final response");
    let CallToolResponse::Complete(result) = response else {
        panic!("detached response must be complete");
    };
    let ContentBlock::Text(content) = &result.content[0] else {
        panic!("detached response must contain text");
    };
    let job_id = content
        .text
        .split_whitespace()
        .find_map(|part| part.strip_prefix("job_id="))
        .expect("detached job id");

    assert_eq!(result.is_error, Some(false));
    assert!(server.detached.status(job_id, 0).is_ok());
    server.detached.terminate_all();
}

#[test]
fn root_capability_blocks_parent_escape() {
    let fixture = tempfile::tempdir().expect("create fixture");
    let root = fixture.path().join("root");
    fs::create_dir(&root).expect("create root");
    fs::write(fixture.path().join("outside.txt"), "outside").expect("write outside");
    let server = AgentShim::from_path(&root).expect("open root");

    let error = server
        .root
        .capability()
        .read_to_string("../outside.txt")
        .expect_err("parent escape must fail");
    assert!(matches!(
        error.kind(),
        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
    ));
}

#[cfg(unix)]
#[test]
fn root_capability_blocks_symlink_escape() {
    use std::os::unix::fs::symlink;

    let fixture = tempfile::tempdir().expect("create fixture");
    let root = fixture.path().join("root");
    fs::create_dir(&root).expect("create root");
    let outside = fixture.path().join("outside.txt");
    fs::write(&outside, "outside").expect("write outside");
    symlink(&outside, root.join("escape")).expect("create symlink");
    let server = AgentShim::from_path(&root).expect("open root");

    server
        .root
        .capability()
        .read_to_string("escape")
        .expect_err("symlink escape must fail");
}

#[cfg(any(unix, windows))]
#[test]
fn root_handle_preserves_repository_identity() {
    let fixture = tempfile::tempdir().expect("create fixture");
    let root = fixture.path().join("root");
    let moved = fixture.path().join("moved");
    fs::create_dir(&root).expect("create root");
    fs::write(root.join("identity.txt"), "original").expect("write original");
    let server = AgentShim::from_path(&root).expect("open root");

    #[cfg(unix)]
    {
        fs::rename(&root, &moved).expect("move original root");
        fs::create_dir(&root).expect("create replacement root");
        fs::write(root.join("identity.txt"), "replacement").expect("write replacement");
    }
    #[cfg(windows)]
    {
        let error = fs::rename(&root, &moved).expect_err("held Windows root blocks replacement");
        assert!(
            matches!(error.raw_os_error(), Some(5 | 32)),
            "unexpected Windows root rename error: {error}"
        );
    }

    assert_eq!(
        server
            .root
            .capability()
            .read_to_string("identity.txt")
            .expect("read held root"),
        "original"
    );
}

/// Re-entrant shutdown: concurrent callers share one transaction and one report, the
/// global token ends up cancelled, and roster admission closes for good.
#[tokio::test]
async fn shutdown_processes_is_idempotent_and_closes_admission() {
    let fixture = tempfile::tempdir().expect("fixture");
    let server = AgentShim::from_path(fixture.path()).expect("server");
    let first = server.clone();
    let second = server.clone();
    let started = std::time::Instant::now();

    let (first, second) = tokio::join!(first.shutdown_processes(), second.shutdown_processes());
    let _ = (first, second);

    assert!(server.shutdown_token().is_cancelled());
    assert!(!server.detached.is_accepting());
    assert!(
        started.elapsed() < std::time::Duration::from_secs(6),
        "overlapping shutdown callers each ran their own cleanup"
    );
}

/// The shutdown transaction waits for foreground owners inside its shared deadline: a
/// held process permit keeps it pending, and releasing the permit lets it finish.
#[tokio::test]
async fn shutdown_waits_for_foreground_owners_to_release() {
    let fixture = tempfile::tempdir().expect("fixture");
    let server = AgentShim::from_path(fixture.path()).expect("server");
    let permit = server
        .resources
        .try_admit_process_for_test()
        .expect("one foreground permit");

    let shutdown = server.clone();
    let waiter = tokio::spawn(async move {
        shutdown.shutdown_processes().await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(
        !waiter.is_finished(),
        "shutdown completed while a foreground owner still held its permit"
    );

    drop(permit);
    tokio::time::timeout(std::time::Duration::from_secs(6), waiter)
        .await
        .expect("shutdown completed after the foreground owner released")
        .expect("shutdown task");
}
