use std::fs;

use rmcp::model::{CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock};
use serde_json::json;

use super::{
    CodexShim, ToolAdmission, ToolAdmissionFailure, blocking_response, diagnostic_tool_error,
    pdf_busy, pdf_timeout, queue_timeout_message, shell_delegate, tool_error,
};
use crate::output::MODEL_BYTE_LIMIT;

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

#[test]
fn pdf_busy_names_the_permit_and_carries_a_retry_delay() {
    for permit in ["pdf_concurrency", "memory_budget"] {
        let details = error_details(pdf_busy(permit));
        assert_eq!(details["code"], "resource_busy");
        assert_eq!(details["retryable"], true);
        assert_eq!(details["details"]["permit"], permit);
        // Without a delay hint the caller can only retry immediately, which spins.
        assert_eq!(
            details["details"]["retry_after_ms"],
            json!(u64::try_from(crate::runtime::PDF_GATE_WAIT.as_millis()).expect("bound"))
        );
    }
}

#[test]
fn occupied_pdf_gate_rejects_pdf_without_blocking_text_reads() {
    let fixture = tempfile::tempdir().expect("fixture");
    fs::write(fixture.path().join("document.pdf"), pdf_fixture()).expect("pdf");
    fs::write(fixture.path().join("notes.txt"), "alpha\nbeta\n").expect("text");
    let server = CodexShim::from_path(fixture.path()).expect("server");
    let _occupied = server
        .resources
        .try_acquire_pdf_gate()
        .expect("PDF gate fixture");

    let CallToolResponse::Complete(text) = read_path_call(&server, "notes.txt") else {
        panic!("text read response must be complete");
    };
    assert_eq!(text.is_error, Some(false));

    let error = error_details(read_path_call(&server, "document.pdf"));
    assert_eq!(error["code"], "resource_busy");
    assert_eq!(error["details"]["permit"], "pdf_concurrency");
}

#[test]
fn pdf_timeout_reports_the_limit_and_that_nothing_was_produced() {
    let details = error_details(pdf_timeout(
        std::time::Duration::from_secs(5),
        std::time::Duration::from_millis(5_120),
    ));
    assert_eq!(details["code"], "resource_timeout");
    assert_eq!(details["retryable"], true);
    assert_eq!(details["details"]["limit_ms"], 5_000);
    assert_eq!(details["details"]["elapsed_ms"], 5_120);
    assert_eq!(details["details"]["work_stopped"], true);
    assert_eq!(details["details"]["partial_output"], false);
}

fn read_path_call(server: &CodexShim, path: &str) -> CallToolResponse {
    let admission = server
        .resources
        .try_admit_read_only()
        .expect("read-only admission");
    let arguments = json!({ "path": path })
        .as_object()
        .expect("arguments object")
        .clone();
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(server.call_read(
            Some(arguments),
            &tokio_util::sync::CancellationToken::new(),
            admission,
            &crate::output::CallOutputBudget::standalone(),
        ))
}

// The second attempt after `file_changed` must not have to re-take the gate. With a
// single slot, re-taking inside the same call would time out and surface as
// `resource_busy`, turning one retryable condition into an unrelated one.
#[test]
fn a_file_changed_retry_holds_the_pdf_gate_instead_of_re_taking_it() {
    let _serialised = crate::tools::read::global_read_state_guard();
    let fixture = tempfile::tempdir().expect("fixture");
    fs::write(fixture.path().join("document.pdf"), pdf_fixture()).expect("pdf");
    let server = CodexShim::from_path(fixture.path()).expect("server");

    let before = server.resources.pdf_gate_acquisitions();
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
        server.resources.pdf_gate_acquisitions() - before,
        1,
        "the gate was taken more than once across the retry loop"
    );
    assert_eq!(server.resources.available_pdf_slots(), 1);
}

// The ceiling is enforced by cancelling at the same checkpoints client cancellation
// uses, because `spawn_blocking` cannot be preempted. A cancellation the server
// initiated must surface as `resource_timeout`, not as the client's cancellation.
#[test]
fn a_mode_runtime_ceiling_reports_resource_timeout_and_frees_the_gate() {
    let _serialised = crate::tools::read::global_read_state_guard();
    let fixture = tempfile::tempdir().expect("fixture");
    fs::write(fixture.path().join("document.pdf"), pdf_fixture()).expect("pdf");
    let server = CodexShim::from_path(fixture.path()).expect("server");

    crate::tools::read::FORCED_PDF_RUNTIME_LIMIT.store(1, std::sync::atomic::Ordering::SeqCst);
    let response = read_path_call(&server, "document.pdf");
    crate::tools::read::FORCED_PDF_RUNTIME_LIMIT.store(0, std::sync::atomic::Ordering::SeqCst);

    let details = error_details(response);
    assert_eq!(details["code"], "resource_timeout");
    assert_eq!(details["retryable"], true);
    assert_eq!(details["details"]["work_stopped"], true);
    assert_eq!(details["details"]["partial_output"], false);
    assert_eq!(
        server.resources.available_pdf_slots(),
        1,
        "a timed-out call must release the gate"
    );
    assert!(
        server
            .resources
            .try_reserve_memory(crate::runtime::DEFAULT_MEMORY_BYTES)
            .is_some(),
        "a timed-out call must release its reservation"
    );
}

#[test]
fn every_pdf_read_path_returns_the_gate_and_its_reservation() {
    let _serialised = crate::tools::read::global_read_state_guard();
    let fixture = tempfile::tempdir().expect("fixture");
    fs::write(fixture.path().join("document.pdf"), pdf_fixture()).expect("pdf");
    fs::write(fixture.path().join("broken.pdf"), b"%PDF-1.7\nnot a pdf\n").expect("broken");
    let server = CodexShim::from_path(fixture.path()).expect("server");
    let free_memory = || {
        server
            .resources
            .try_reserve_memory(crate::runtime::DEFAULT_MEMORY_BYTES)
            .is_some()
    };

    for path in ["document.pdf", "broken.pdf", "missing.pdf"] {
        let _ = read_path_call(&server, path);
        assert_eq!(
            server.resources.available_pdf_slots(),
            1,
            "{path} leaked the PDF gate"
        );
        assert!(free_memory(), "{path} leaked its memory reservation");
    }
}

#[test]
fn process_queue_timeout_does_not_claim_process_diagnostics() {
    let message = queue_timeout_message("run_program", 25);
    assert!(message.contains("no child was started"));
    for field in ["Resolved program:", "Launcher:", "Cwd:", "Exit code:"] {
        assert!(!message.contains(field));
    }
}

#[test]
fn tool_errors_are_bounded() {
    let CallToolResponse::Complete(result) =
        tool_error("validation", false, "界".repeat(40_000), None)
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
    assert!(crate::output::tool_result_fits_budget(
        text,
        Some(structured),
        true
    ));
    assert!(
        crate::output::tool_result_encoded_len(text, Some(structured), true) <= MODEL_BYTE_LIMIT
    );
}

#[test]
fn tool_error_budget_counts_escaped_text_and_bounds_detail_captures() {
    let details = json!({
        "stdout": { "text": "\\\"\u{1}".repeat(20_000), "total_bytes": 60_000 },
        "termination_outcome": "terminated"
    });
    let CallToolResponse::Complete(result) = tool_error(
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
    assert!(crate::output::tool_result_fits_budget(
        &content.text,
        Some(structured),
        true
    ));
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
    let budget = crate::output::CallOutputBudget::standalone();
    let CallToolResponse::Complete(result) = blocking_response::<crate::tools::exec::ProcessError>(
        "run_program",
        3,
        Ok(Ok(output)),
        &gate,
        &cancellation,
        &budget,
    ) else {
        panic!("tool response must be complete");
    };

    let ContentBlock::Text(content) = &result.content[0] else {
        panic!("success response must contain text");
    };
    assert_eq!(content.text, "summary");
    assert_eq!(result.structured_content, None);
    assert_eq!(result.is_error, Some(false));
}

#[test]
fn final_verifier_replaces_an_unbounded_model_payload() {
    let fixture = tempfile::tempdir().expect("fixture");
    let server = CodexShim::from_path(fixture.path()).expect("server");
    let budget = crate::output::CallOutputBudget::standalone();
    let verified = blocking_response::<crate::tools::exec::ProcessError>(
        "run_program",
        3,
        Ok(Ok(crate::tools::ToolOutput::new(" x".repeat(10_000)))),
        &server.output_token_gate,
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
    let spent = crate::output::CallOutputBudget::new(token_gate.clone(), burst_gate.begin_call());
    assert_eq!(spent.ceiling(), 2_048);
    spent.finish(2_048, false);
    let budget = crate::output::CallOutputBudget::new(token_gate, burst_gate.begin_call());
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

#[test]
fn unavailable_bash_response_is_io_and_not_retryable() {
    let error = crate::tools::exec::ProcessError::Unavailable("no GNU bash".to_owned());
    let CallToolResponse::Complete(result) = diagnostic_tool_error(&error) else {
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
    let error = crate::tools::grep::GrepError::Unsearchable(crate::output::SkipReason::Binary);
    let CallToolResponse::Complete(result) = diagnostic_tool_error(&error) else {
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
    let error = crate::tools::grep::GrepError::Unsearchable(
        crate::output::SkipReason::ChangedWhileSearched,
    );
    let CallToolResponse::Complete(result) = diagnostic_tool_error(&error) else {
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
    let server = CodexShim::builder(fixture.path())
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
    let server = CodexShim::builder(fixture.path())
        .expect("builder")
        .runtime_limits(runtime)
        .build()
        .expect("server");
    let foreground = server
        .resources
        .try_admit_process()
        .expect("foreground admission");

    assert!(server.resources.try_admit_process().is_none());
    let detached = server
        .try_admit_tool(&detached_request())
        .expect("detached admission remains independent");
    assert!(matches!(detached, ToolAdmission::Detached(_)));
    drop(foreground);
}

#[test]
fn root_capability_blocks_parent_escape() {
    let fixture = tempfile::tempdir().expect("create fixture");
    let root = fixture.path().join("root");
    fs::create_dir(&root).expect("create root");
    fs::write(fixture.path().join("outside.txt"), "outside").expect("write outside");
    let server = CodexShim::from_path(&root).expect("open root");

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
    let server = CodexShim::from_path(&root).expect("open root");

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
    let server = CodexShim::from_path(&root).expect("open root");

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
    let server = CodexShim::from_path(fixture.path()).expect("server");
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
    let server = CodexShim::from_path(fixture.path()).expect("server");
    let permit = server
        .resources
        .try_admit_process()
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
