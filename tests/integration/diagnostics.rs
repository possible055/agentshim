use super::common::{fixtures::*, session::*};
use super::*;

#[test]
fn multiple_servers_share_complete_jsonl_without_mixing_call_identity() {
    let directory = tempfile::tempdir().expect("log directory");
    let mut sessions = (0..4)
        .map(|_| {
            TestSession::builder()
                .log_dir(directory.path())
                .log_mode("errors")
                .spawn()
        })
        .collect::<Vec<_>>();
    for (index, session) in sessions.iter_mut().enumerate() {
        session.call_invalid_read(index as u64 + 1);
    }

    let deadline = Instant::now() + Duration::from_secs(3);
    let recs = loop {
        let recs = records(directory.path());
        if recs
            .iter()
            .filter(|record| record["event"] == "tool_error")
            .count()
            == sessions.len()
        {
            break recs;
        }
        assert!(
            Instant::now() < deadline,
            "diagnostic batches were not persisted"
        );
        thread::sleep(Duration::from_millis(10));
    };

    let errors = recs
        .iter()
        .filter(|record| record["event"] == "tool_error")
        .collect::<Vec<_>>();
    assert!(recs.iter().all(|record| record["schema_version"] == 1));
    assert_eq!(
        errors
            .iter()
            .filter_map(|record| record["instance_id"].as_str())
            .collect::<BTreeSet<_>>()
            .len(),
        sessions.len()
    );
    assert_eq!(
        errors
            .iter()
            .filter_map(|record| record["call_id"].as_str())
            .collect::<BTreeSet<_>>()
            .len(),
        sessions.len()
    );
    assert!(
        errors
            .iter()
            .all(|record| record["error_class"] == "validation")
    );

    for session in sessions {
        session.close();
    }
}

#[test]
fn shell_delegate_log_event_contains_only_the_derived_classification() {
    let directory = tempfile::tempdir().expect("log directory");
    let mut session = TestSession::builder()
        .log_dir(directory.path())
        .log_mode("errors")
        .spawn();
    let command = "pwsh -NoProfile -Command private_argument C:/private/path";
    session.call(
        1,
        "bash",
        &json!({ "command": command, "timeout_ms": 0 }),
        true,
    );
    session.close();

    let recs = records(directory.path());
    let event = recs
        .iter()
        .find(|record| record["event"] == "tool_start" && record["tool"] == "bash")
        .expect("bash tool_start event");
    assert_eq!(event["shell_delegate"], "pwsh");
    for key in ["command", "arguments", "path", "token", "argv"] {
        assert!(event.get(key).is_none(), "sensitive field present: {key}");
    }
    let serialized = serde_json::to_string(event).expect("serialize event");
    for sensitive in [command, "private_argument", "C:/private/path"] {
        assert!(
            !serialized.contains(sensitive),
            "event contains sensitive input: {sensitive}"
        );
    }
}

#[test]
fn unavailable_log_directory_does_not_corrupt_json_rpc_stdout() {
    let directory = tempfile::tempdir().expect("directory");
    let blocked = directory.path().join("not-a-directory");
    fs::write(&blocked, b"file").expect("blocking file");
    let mut session = TestSession::builder()
        .log_dir(&blocked)
        .log_mode("errors")
        .spawn();
    session.call_invalid_read(1);
    session.close();
}

#[test]
fn all_mode_persists_modern_discovery_and_tool_list_metadata() {
    let directory = tempfile::tempdir().expect("log directory");
    let mut session = TestSession::builder()
        .log_dir(directory.path())
        .log_mode("all")
        .spawn();

    session.send(&modern_request(1, "server/discover", empty_params()));
    assert_eq!(session.receive()["id"], 1);
    session.send(&modern_request(2, "tools/list", empty_params()));
    assert_eq!(session.receive()["id"], 2);
    session.close();

    let recs = records(directory.path());
    let discover = recs
        .iter()
        .find(|record| record["event"] == "discover")
        .expect("discover event");
    assert_eq!(discover["protocol"], "2026-07-28");
    assert_eq!(discover["client_name"], "agentshim-wire-test");
    assert_eq!(discover["client_version"], "1.0.0");

    let tools_list = recs
        .iter()
        .find(|record| record["event"] == "tools_list")
        .expect("tools list event");
    assert_eq!(tools_list["protocol"], "2026-07-28");
    assert_eq!(tools_list["tool_count"], 6);
    assert_eq!(
        tools_list["toolset"],
        "read,grep,glob,run_program,bash,bash_status"
    );
    assert_eq!(tools_list["has_cursor"], false);
    assert_eq!(tools_list["cache_ttl_ms"], 300_000);
    assert_eq!(tools_list["cache_scope"], "private");
    let request_id = tools_list["request_id"]
        .as_str()
        .expect("opaque request ID");
    assert!(uuid::Uuid::parse_str(request_id).is_ok());
    let sent = recs
        .iter()
        .find(|record| record["event"] == "tools_list_sent")
        .expect("tools list sent event");
    assert_eq!(sent["request_id"], request_id);
    assert!(recs.iter().any(|record| {
        record["event"] == "server_stop"
            && record["outcome"] == "success"
            && record["reason"] == "transport_closed"
    }));
}

#[test]
fn all_mode_persists_legacy_initialization_and_tool_list_metadata() {
    let directory = tempfile::tempdir().expect("log directory");
    let mut session = TestSession::builder()
        .log_dir(directory.path())
        .log_mode("all")
        .spawn();

    session.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": { "name": "diagnostics-legacy", "version": "1.0.0" }
        }
    }));
    assert_eq!(session.receive()["id"], 1);
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
    assert_eq!(session.receive()["id"], 2);
    session.close();

    let recs = records(directory.path());
    let initialize = recs
        .iter()
        .find(|record| record["event"] == "initialize")
        .expect("initialize event");
    assert_eq!(initialize["protocol"], "2025-11-25");
    assert_eq!(initialize["client_name"], "diagnostics-legacy");
    assert_eq!(initialize["client_version"], "1.0.0");
    assert!(recs.iter().any(|record| record["event"] == "initialized"));

    let tools_list = recs
        .iter()
        .find(|record| record["event"] == "tools_list")
        .expect("tools list event");
    assert_eq!(tools_list["protocol"], "2025-11-25");
    assert_eq!(tools_list["client_name"], "diagnostics-legacy");
    assert_eq!(tools_list["tool_count"], 6);
    let request_id = tools_list["request_id"]
        .as_str()
        .expect("opaque request ID");
    assert_eq!(
        recs.iter()
            .find(|record| record["event"] == "tools_list_sent")
            .expect("tools list sent event")["request_id"],
        request_id
    );
    assert!(recs.iter().any(|record| {
        record["event"] == "server_stop"
            && record["outcome"] == "success"
            && record["reason"] == "transport_closed"
    }));
}

#[test]
fn tools_list_delivery_uses_an_opaque_correlation_id() {
    const SECRET_ID: &str = "private-client-request-id";

    let directory = tempfile::tempdir().expect("log directory");
    let mut session = TestSession::builder()
        .log_dir(directory.path())
        .log_mode("all")
        .spawn();
    let mut request = modern_request(1, "tools/list", empty_params());
    request["id"] = json!(SECRET_ID);
    session.send(&request);
    assert_eq!(session.receive()["id"], SECRET_ID);
    session.close();

    let recs = records(directory.path());
    let serialized = serde_json::to_string(&recs).expect("serialize records");
    assert!(!serialized.contains(SECRET_ID));
    let tools_list = recs
        .iter()
        .find(|record| record["event"] == "tools_list")
        .expect("tools list event");
    let request_id = tools_list["request_id"]
        .as_str()
        .expect("opaque request ID");
    assert!(uuid::Uuid::parse_str(request_id).is_ok());
    assert_eq!(
        recs.iter()
            .find(|record| record["event"] == "tools_list_sent")
            .expect("tools list sent event")["request_id"],
        request_id
    );
}

#[test]
fn tools_list_stdout_failure_is_correlated_and_stops_the_server() {
    const SECRET_ID: &str = "private-failed-request-id";

    let directory = tempfile::tempdir().expect("log directory");
    let mut session = TestSession::builder()
        .log_dir(directory.path())
        .log_mode("errors")
        .spawn();
    session.close_stdout();
    let mut request = modern_request(1, "tools/list", empty_params());
    request["id"] = json!(SECRET_ID);
    session.send(&request);
    let status = session.wait_for_exit(Duration::from_secs(10));
    assert!(!status.success());

    let recs = records(directory.path());
    let serialized = serde_json::to_string(&recs).expect("serialize records");
    assert!(!serialized.contains(SECRET_ID));
    let tools_list = recs
        .iter()
        .find(|record| record["event"] == "tools_list")
        .expect("tools list event");
    assert_eq!(tools_list["context"], true);
    let request_id = tools_list["request_id"]
        .as_str()
        .expect("opaque request ID");
    let write_error = recs
        .iter()
        .find(|record| record["event"] == "stdout_write_error")
        .expect("stdout write error");
    assert_eq!(write_error["request_id"], request_id);
    assert_eq!(write_error["outcome"], "error");
    assert!(recs.iter().any(|record| {
        record["event"] == "server_stop"
            && record["outcome"] == "error"
            && record["error_class"] == "transport"
    }));
}

#[test]
fn errors_mode_keeps_successful_handshakes_in_memory_until_an_error() {
    let directory = tempfile::tempdir().expect("log directory");
    let mut session = TestSession::builder()
        .log_dir(directory.path())
        .log_mode("errors")
        .spawn();

    session.send(&modern_request(1, "server/discover", empty_params()));
    assert_eq!(session.receive()["id"], 1);
    session.send(&modern_request(2, "tools/list", empty_params()));
    assert_eq!(session.receive()["id"], 2);
    session.close();
    assert!(jsonl_paths(directory.path()).is_empty());

    let mut session = TestSession::builder()
        .log_dir(directory.path())
        .log_mode("errors")
        .spawn();
    session.send(&modern_request(3, "server/discover", empty_params()));
    assert_eq!(session.receive()["id"], 3);
    session.send(&modern_request(4, "tools/list", empty_params()));
    assert_eq!(session.receive()["id"], 4);
    session.call_invalid_read(5);
    session.close();

    let recs = records(directory.path());
    assert!(
        recs.iter()
            .any(|record| record["event"] == "discover" && record["context"] == true)
    );
    assert!(
        recs.iter()
            .any(|record| record["event"] == "tools_list" && record["context"] == true)
    );
    assert!(recs.iter().any(|record| record["event"] == "tool_error"));
}

#[test]
fn oversized_frame_logs_a_safe_rejection_record() {
    const SECRET: &str = "diagnostics-stdio-secret";

    let directory = tempfile::tempdir().expect("log directory");
    let mut session = TestSession::builder()
        .log_dir(directory.path())
        .log_mode("errors")
        .spawn();
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list",
        "params": {
            "secret": SECRET,
            "padding": "x".repeat(8 * 1024 * 1024)
        }
    });
    session.send(&request);
    session.stdin.take();

    let deadline = Instant::now() + Duration::from_secs(3);
    let status = loop {
        if let Some(status) = session.child.try_wait().expect("poll server") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "server did not reject oversized frame"
        );
        thread::sleep(Duration::from_millis(10));
    };
    assert!(!status.success());

    let recs = records(directory.path());
    let rejection = recs
        .iter()
        .find(|record| record["event"] == "mcp_frame_rejected")
        .expect("frame rejection event");
    assert_eq!(rejection["reason"], "frame_too_large");
    assert_eq!(rejection["frame_limit_bytes"], 8 * 1024 * 1024);
    assert!(recs.iter().any(|record| {
        record["event"] == "server_stop"
            && record["outcome"] == "error"
            && record["error_class"] == "transport"
    }));
    assert!(
        !serde_json::to_string(&recs)
            .expect("serialize records")
            .contains(SECRET)
    );
}

#[test]
fn missing_modern_metadata_logs_only_safe_framework_classification() {
    const SECRET: &str = "diagnostics-metadata-secret";

    let directory = tempfile::tempdir().expect("log directory");
    let mut session = TestSession::builder()
        .log_dir(directory.path())
        .log_mode("errors")
        .spawn();
    session.send(&modern_request(1, "server/discover", empty_params()));
    assert_eq!(session.receive()["id"], 1);
    session.send(&json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": { "secret": SECRET }
    }));
    let response = session.receive();
    assert_eq!(response["id"], 2);
    assert_eq!(response["error"]["code"], -32602);
    session.close();

    let recs = records(directory.path());
    let framework_error = recs
        .iter()
        .find(|record| record["event"] == "rmcp_internal")
        .expect("rmcp error event");
    assert_eq!(framework_error["framework"], "rmcp");
    assert!(
        framework_error["framework_target"]
            .as_str()
            .is_some_and(|target| !target.is_empty())
    );
    assert!(
        framework_error["framework_event"]
            .as_str()
            .is_some_and(|event| !event.is_empty())
    );
    assert!(
        !serde_json::to_string(&recs)
            .expect("serialize records")
            .contains(SECRET)
    );
}

#[test]
fn status_and_purge_report_storage_and_preserve_the_active_log() {
    let directory = tempfile::tempdir().expect("log directory");
    let today = Utc::now().date_naive();
    let old = today.checked_sub_days(Days::new(31)).expect("old date");
    let active = directory
        .path()
        .join(format!("agentshim-{today}.0001.jsonl"));
    let expired = directory.path().join(format!("agentshim-{old}.0001.jsonl"));
    fs::write(&active, b"{}\n").expect("active log");
    fs::write(&expired, b"{}\n").expect("expired log");

    let status = Command::new(env!("CARGO_BIN_EXE_agentshim"))
        .args(["logs", "status"])
        .env("AGENTSHIM_LOG_MODE", "errors")
        .env("AGENTSHIM_LOG_DIR", directory.path())
        .output()
        .expect("status");
    assert!(status.status.success());
    let status_output = String::from_utf8(status.stdout).expect("status UTF-8");
    assert!(status_output.contains("JSONL files: 2"));
    assert!(status_output.contains("retention days: 30"));
    assert!(status_output.contains("recorded dropped records: 0"));

    let purge = Command::new(env!("CARGO_BIN_EXE_agentshim"))
        .args(["logs", "purge"])
        .env("AGENTSHIM_LOG_MODE", "errors")
        .env("AGENTSHIM_LOG_DIR", directory.path())
        .output()
        .expect("purge");
    assert!(purge.status.success());
    assert!(
        String::from_utf8(purge.stdout)
            .expect("purge UTF-8")
            .contains("deleted files: 1")
    );
    assert!(active.exists());
    assert!(!expired.exists());
}
