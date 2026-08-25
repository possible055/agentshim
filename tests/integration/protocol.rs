use super::common::{fixtures::*, session::*};
use super::*;

#[test]
fn modern_lifecycle_serves_a_tool_call_and_shuts_down_at_eof() {
    let mut session = TestSession::start();

    session.send(&modern_request(1, "server/discover", empty_params()));
    let discover = session.receive();
    assert_eq!(discover["id"], 1);
    assert_eq!(
        discover["result"]["supportedVersions"],
        json!([
            "2026-07-28",
            "2025-11-25",
            "2025-06-18",
            "2025-03-26",
            "2024-11-05"
        ])
    );
    assert_eq!(discover["result"]["capabilities"], json!({ "tools": {} }));

    session.send(&modern_request(2, "tools/list", empty_params()));
    let list = session.receive();
    assert_eq!(list["id"], 2);
    assert_eq!(
        list["result"]["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .map(|tool| tool["name"].as_str().expect("tool name"))
            .collect::<Vec<_>>(),
        ["read", "grep", "glob", "run_program", "bash", "bash_status"]
    );

    let mut call = empty_params();
    call.insert("name".to_owned(), json!("read"));
    call.insert("arguments".to_owned(), json!({ "path": "src/main.rs" }));
    session.send(&modern_request(3, "tools/call", call));
    let response = session.receive();
    assert_eq!(response["id"], 3);
    assert_eq!(response["result"]["resultType"], "complete");
    assert_eq!(response["result"]["isError"], false);
    let read_text = response["result"]["content"][0]["text"]
        .as_str()
        .expect("read text");
    assert!(!read_text.contains("Path: "));
    assert!(!read_text.is_empty());
    assert!(!read_text.contains("Complete."));
    assert!(
        response["result"].get("structuredContent").is_none(),
        "read success must not emit structured content"
    );

    session.close();
}

#[test]
#[ignore = "manual rmcp response-retention soak"]
fn sequential_five_thousand_stdio_requests_reach_a_resource_plateau() {
    let mut session = TestSession::start();
    #[cfg(windows)]
    let mut samples = Vec::new();
    for id in 1..=5_000_u64 {
        session.send(&modern_request(id, "server/discover", empty_params()));
        assert_eq!(session.receive()["id"], id);
        #[cfg(windows)]
        if id.is_multiple_of(500) {
            let (memory, handles) = sample_server_resources(session.pid());
            samples.push((
                usize::try_from(id).expect("bounded request count"),
                memory,
                handles,
            ));
        }
    }
    #[cfg(windows)]
    {
        let memory = samples
            .iter()
            .map(|(requests, memory, _)| (*requests, *memory))
            .collect::<Vec<_>>();
        let handles = samples
            .iter()
            .map(|(requests, _, handles)| (*requests, u64::from(*handles)))
            .collect::<Vec<_>>();
        assert!(
            metric_slope(&memory) < 1_024.0,
            "working set retained at least 1 KiB per completed request: {memory:?}"
        );
        assert!(
            metric_slope(&handles) <= 0.01,
            "handle count grew with completed requests: {handles:?}"
        );
    }
    session.close();
}

#[cfg(windows)]
#[test]
#[ignore = "manual rmcp stalled-stdout retention soak"]
fn unread_stdout_hits_a_bounded_backlog_and_shuts_down() {
    let mut session = TestSession::start();
    session.handshake();
    let mut stdin = session.stdin.take().expect("server stdin");
    let writer = std::thread::spawn(move || {
        let mut sent = 0_usize;
        for id in 2..=5_001_u64 {
            if serde_json::to_writer(
                &mut stdin,
                &modern_request(id, "server/discover", empty_params()),
            )
            .is_err()
                || stdin.write_all(b"\n").is_err()
                || stdin.flush().is_err()
            {
                break;
            }
            sent += 1;
        }
        sent
    });
    let baseline = sample_server_resources(session.pid());
    let mut samples = vec![baseline];
    for _ in 0..4 {
        std::thread::sleep(Duration::from_secs(1));
        if session.child.try_wait().expect("poll server").is_some() {
            break;
        }
        samples.push(sample_server_resources(session.pid()));
    }
    let status = session.wait_for_exit(Duration::from_secs(10));
    let sent = writer.join().expect("request writer");
    let memory = samples
        .iter()
        .map(|(memory, _)| *memory)
        .collect::<Vec<_>>();
    let handles = samples
        .iter()
        .map(|(_, handles)| *handles)
        .collect::<Vec<_>>();
    eprintln!("unread stdout sent requests: {sent}");
    eprintln!("unread stdout working-set samples: {memory:?}");
    eprintln!("unread stdout handle samples: {handles:?}");
    assert!(!status.success(), "backlog shutdown exited successfully");
    assert!(
        memory.iter().max().expect("memory sample") - memory.iter().min().expect("memory sample")
            < 32 * 1024 * 1024,
        "unread stdout exceeded the bounded working-set tolerance: {memory:?}"
    );
    assert!(
        handles.iter().max().expect("handle sample") - handles.iter().min().expect("handle sample")
            <= 16,
        "unread stdout grew too many process handles: {handles:?}"
    );
}

#[test]
#[ignore = "manual rmcp slow-stdout recovery soak"]
fn a_slow_stdout_reader_continues_serving_within_the_backlog_limit() {
    let mut session = TestSession::start();
    session.handshake();
    let mut stdout = session.stdout.take().expect("server stdout");
    let reader = std::thread::spawn(move || {
        let mut received = 0_usize;
        let mut line = String::new();
        loop {
            line.clear();
            match stdout.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    serde_json::from_str::<Value>(&line).expect("response JSON");
                    received += 1;
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("read response: {error}"),
            }
        }
        received
    });
    for id in 2..=501_u64 {
        session.send(&modern_request(id, "server/discover", empty_params()));
        std::thread::sleep(Duration::from_millis(2));
    }
    session.stdin.take();
    let status = session.wait_for_exit(Duration::from_secs(10));
    let received = reader.join().expect("response reader");
    assert!(status.success(), "slow-reader server exited with {status}");
    assert_eq!(received, 500);
}

#[cfg(windows)]
#[expect(
    clippy::cast_precision_loss,
    reason = "resource samples are bounded test measurements used only for regression slope checks"
)]
fn metric_slope(samples: &[(usize, u64)]) -> f64 {
    let count = samples.len() as f64;
    let mean_x = samples.iter().map(|(x, _)| *x as f64).sum::<f64>() / count;
    let mean_y = samples.iter().map(|(_, y)| *y as f64).sum::<f64>() / count;
    let (numerator, denominator) =
        samples
            .iter()
            .fold((0.0, 0.0), |(numerator, denominator), (x, y)| {
                let x = *x as f64 - mean_x;
                (numerator + x * (*y as f64 - mean_y), denominator + x * x)
            });
    numerator / denominator
}

#[cfg(windows)]
fn sample_server_resources(pid: u32) -> (u64, u32) {
    use std::mem;
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{
            GetProcessHandleCount, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
        },
    };

    #[repr(C)]
    struct ProcessMemoryCounters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn K32GetProcessMemoryInfo(
            process: windows_sys::Win32::Foundation::HANDLE,
            counters: *mut ProcessMemoryCounters,
            size: u32,
        ) -> i32;
    }

    // Safety: the PID belongs to the live child owned by `session`.
    let process =
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ, 0, pid) };
    assert!(!process.is_null(), "open server process");
    // Safety: the zeroed C structure receives its size before the API call.
    let mut counters: ProcessMemoryCounters = unsafe { mem::zeroed() };
    counters.cb = u32::try_from(mem::size_of::<ProcessMemoryCounters>()).expect("counter size");
    // Safety: the handle is live and the output pointer remains valid for the call.
    let memory_result = unsafe { K32GetProcessMemoryInfo(process, &raw mut counters, counters.cb) };
    assert_ne!(memory_result, 0);
    let mut handles = 0_u32;
    // Safety: the handle is live and `handles` is a writable output parameter.
    let handle_result = unsafe { GetProcessHandleCount(process, &raw mut handles) };
    assert_ne!(handle_result, 0);
    // Safety: this function owns the query handle and closes it exactly once.
    unsafe { CloseHandle(process) };
    (counters.working_set_size as u64, handles)
}

#[test]
fn oversized_valid_frame_closes_stdio_transport() {
    let mut session = TestSession::start();
    let mut params = empty_params();
    params.insert("padding".to_owned(), json!(""));
    let mut request = modern_request(1, "server/discover", params);
    let base_length = serde_json::to_vec(&request).expect("base request").len();
    let padding_length = MAX_RECEIVE_FRAME_BYTES + 1 - base_length;
    request["params"]["padding"] = json!("x".repeat(padding_length));
    let encoded = serde_json::to_vec(&request).expect("oversized request");
    assert_eq!(encoded.len(), MAX_RECEIVE_FRAME_BYTES + 1);

    let stdin = session.stdin.as_mut().expect("server stdin");
    stdin.write_all(&encoded).expect("write oversized frame");
    stdin.write_all(b"\n").expect("write frame delimiter");
    stdin.flush().expect("flush oversized frame");
    session.stdin.take();

    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = session.child.try_wait().expect("poll server") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = session.child.kill();
            panic!("server did not close transport after oversized frame");
        }
        thread::sleep(Duration::from_millis(10));
    };
    assert!(
        !status.success(),
        "oversized transport failure must be reported"
    );
    let mut line = String::new();
    assert_eq!(
        session
            .stdout
            .as_mut()
            .expect("stdout open")
            .read_line(&mut line)
            .expect("read closed stdout"),
        0
    );
}

#[test]
fn missing_modern_metadata_is_rejected_without_corrupting_stdio() {
    let mut session = TestSession::start();
    session.send(&modern_request(1, "server/discover", empty_params()));
    assert_eq!(session.receive()["id"], 1);

    session.send(&json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    }));
    let error = session.receive();
    assert_eq!(error["id"], 2);
    assert_eq!(error["error"]["code"], -32602);

    session.send(&modern_request(3, "tools/list", empty_params()));
    assert_eq!(session.receive()["id"], 3);
    session.close();
}

#[test]
fn initialize_uses_the_native_legacy_lifecycle() {
    let mut session = TestSession::start();
    session.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "legacy-test", "version": "1.0.0" }
        }
    }));
    let initialize = session.receive();
    assert_eq!(initialize["id"], 1);
    assert_eq!(initialize["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(initialize["result"]["capabilities"], json!({ "tools": {} }));

    session.send(&json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    }));
    session.send(&json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    }));
    let list = session.receive();
    assert_eq!(list["id"], 2);
    assert_eq!(
        list["result"]["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .map(|tool| tool["name"].as_str().expect("tool name"))
            .collect::<Vec<_>>(),
        ["read", "grep", "glob", "run_program", "bash", "bash_status"]
    );

    session.send(&json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "read",
            "arguments": { "path": "src/main.rs", "line_count": 2 }
        }
    }));
    let read = session.receive();
    assert_eq!(read["id"], 3);
    assert_eq!(read["result"]["isError"], false);
    assert!(read["result"].get("resultType").is_none());
    assert!(
        read["result"]["content"][0]["text"]
            .as_str()
            .expect("read text")
            .contains("1\tmod cli;")
    );
    assert!(
        read["result"].get("structuredContent").is_none(),
        "legacy read success must not emit structured content"
    );
    session.close();
}

#[test]
fn initialize_accepts_all_supported_versions() {
    for protocol_version in [
        "2026-07-28",
        "2025-11-25",
        "2025-06-18",
        "2025-03-26",
        "2024-11-05",
    ] {
        let mut session = TestSession::start();
        session.send(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": protocol_version,
                "capabilities": {},
                "clientInfo": { "name": "legacy-test", "version": "1.0.0" }
            }
        }));
        let initialize = session.receive();
        assert_eq!(initialize["id"], 1);
        assert_eq!(initialize["result"]["protocolVersion"], protocol_version);

        session.send(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }));
        session.send(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        }));
        let list = session.receive();
        assert_eq!(list["id"], 2);
        assert_eq!(list["result"]["tools"].as_array().map(Vec::len), Some(6));
        session.close();
    }
}

#[test]
fn initialize_unknown_version_falls_back_without_method_error() {
    let mut session = TestSession::start();
    session.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2030-01-01",
            "capabilities": {},
            "clientInfo": { "name": "future-test", "version": "1.0.0" }
        }
    }));
    let initialize = session.receive();
    assert_eq!(initialize["id"], 1);
    assert_eq!(initialize["result"]["protocolVersion"], "2026-07-28");

    session.send(&json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized"
    }));
    session.send(&json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    }));
    let list = session.receive();
    assert_eq!(list["id"], 2);
    assert_eq!(list["result"]["tools"].as_array().map(Vec::len), Some(6));
    session.close();
}
