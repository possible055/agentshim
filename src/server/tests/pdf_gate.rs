use super::*;

fn pdf_fixture() -> Vec<u8> {
    agentshim_core::tools::read::minimal_pdf(b"BT /F1 18 Tf 20 150 Td (PDF gate probe) Tj ET")
}

struct ForcedPdfHooks;

impl ForcedPdfHooks {
    fn install(runtime_limit_ms: u64, block_ms: u64) -> Self {
        crate::tools::read::FORCED_PDF_RUNTIME_LIMIT
            .store(runtime_limit_ms, std::sync::atomic::Ordering::SeqCst);
        crate::tools::read::FORCED_PDF_BLOCK_MS
            .store(block_ms, std::sync::atomic::Ordering::SeqCst);
        Self
    }
}

impl Drop for ForcedPdfHooks {
    fn drop(&mut self) {
        crate::tools::read::FORCED_PDF_RUNTIME_LIMIT.store(0, std::sync::atomic::Ordering::SeqCst);
        crate::tools::read::FORCED_PDF_BLOCK_MS.store(0, std::sync::atomic::Ordering::SeqCst);
    }
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

// The ceiling returns control to the client without preempting the dedicated PDF
// worker. That worker must retain its permits until it actually stops.
#[test]
fn a_mode_runtime_ceiling_reports_resource_timeout_and_retains_permits_until_completion() {
    let _serialised = crate::tools::read::global_read_state_guard();
    let fixture = tempfile::tempdir().expect("fixture");
    fs::write(fixture.path().join("document.pdf"), pdf_fixture()).expect("pdf");
    let server = AgentShim::from_path(fixture.path()).expect("server");

    let _hooks = ForcedPdfHooks::install(20, 250);
    let response = read_path_call(&server, "document.pdf");

    let details = error_details(response);
    assert_eq!(details["code"], "resource_timeout");
    assert_eq!(details["retryable"], true);
    assert_eq!(details["details"]["work_stopped"], true);
    assert_eq!(details["details"]["partial_output"], false);
    assert_eq!(
        server.tool_engine.available_pdf_slots_for_test(),
        0,
        "the running worker released the gate before it stopped"
    );
    assert!(
        server.tool_engine.available_memory_bytes_for_test() < server.runtime_limits().memory_bytes,
        "the running worker released its reservation before it stopped"
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while server.tool_engine.available_pdf_slots_for_test() == 0
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(
        server.tool_engine.available_pdf_slots_for_test(),
        1,
        "the completed worker did not release the gate"
    );
    assert_eq!(
        server.tool_engine.available_memory_bytes_for_test(),
        server.runtime_limits().memory_bytes,
        "the completed worker did not release its reservation"
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
