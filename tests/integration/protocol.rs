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
