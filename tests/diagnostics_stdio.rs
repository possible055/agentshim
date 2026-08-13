use std::{
    collections::BTreeSet,
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use chrono::{Days, Utc};
use serde_json::{Value, json};

struct Session {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Option<BufReader<ChildStdout>>,
}

impl Session {
    fn start(log_directory: &Path) -> Self {
        Self::start_with_mode(log_directory, "errors")
    }

    fn start_with_mode(log_directory: &Path, mode: &str) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_codexshim"))
            .arg("serve")
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .env("CODEXSHIM_LOG_MODE", mode)
            .env("CODEXSHIM_LOG_DIR", log_directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start codexshim");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        Self {
            child,
            stdin: Some(stdin),
            stdout: Some(stdout),
        }
    }

    fn call_invalid_read(&mut self, id: u64) {
        self.call_read(id, &json!(1), true);
    }

    fn call_read(&mut self, id: u64, path: &Value, expected_error: bool) {
        self.call(id, "read", &json!({ "path": path }), expected_error);
    }

    fn call(&mut self, id: u64, name: &str, arguments: &Value, expected_error: bool) {
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {
                "name": name,
                "arguments": arguments,
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientInfo": {
                        "name": "diagnostics-test",
                        "version": "1.0.0"
                    },
                    "io.modelcontextprotocol/clientCapabilities": {}
                }
            }
        });
        self.send(&request);

        let response = self.receive();
        assert_eq!(response["id"], id);
        assert_eq!(response["result"]["isError"], expected_error);
    }

    fn send(&mut self, request: &Value) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        serde_json::to_writer(&mut *stdin, &request).expect("request");
        stdin.write_all(b"\n").expect("newline");
        stdin.flush().expect("flush");
    }

    fn receive(&mut self) -> Value {
        let mut line = String::new();
        self.stdout
            .as_mut()
            .expect("stdout open")
            .read_line(&mut line)
            .expect("response");
        serde_json::from_str(&line).expect("JSON-RPC response")
    }

    fn close_stdout(&mut self) {
        self.stdout.take();
    }

    fn wait_for_exit(&mut self) -> std::process::ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().expect("poll server") {
                return status;
            }
            assert!(Instant::now() < deadline, "server did not exit");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn close(mut self) {
        self.stdin.take();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(status) = self.child.try_wait().expect("poll server") {
                assert!(status.success(), "server exited with {status}");
                return;
            }
            if Instant::now() >= deadline {
                self.child.kill().expect("kill server");
                panic!("server did not exit at EOF");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stdin.take();
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn jsonl_paths(directory: &Path) -> Vec<PathBuf> {
    fs::read_dir(directory)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .collect()
}

fn records(directory: &Path) -> Vec<Value> {
    jsonl_paths(directory)
        .iter()
        .flat_map(|path| {
            BufReader::new(fs::File::open(path).expect("log"))
                .lines()
                .map(|line| {
                    serde_json::from_str::<Value>(&line.expect("complete line"))
                        .expect("complete JSON")
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

fn modern_request(id: u64, method: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": {
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientInfo": {
                    "name": "diagnostics-modern",
                    "version": "1.0.0"
                },
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        }
    })
}

#[test]
fn multiple_servers_share_complete_jsonl_without_mixing_call_identity() {
    let directory = tempfile::tempdir().expect("log directory");
    let mut sessions = (0..4)
        .map(|_| Session::start(directory.path()))
        .collect::<Vec<_>>();
    for (index, session) in sessions.iter_mut().enumerate() {
        session.call_invalid_read(index as u64 + 1);
    }

    let deadline = Instant::now() + Duration::from_secs(3);
    let records = loop {
        let records = records(directory.path());
        if records
            .iter()
            .filter(|record| record["event"] == "tool_error")
            .count()
            == sessions.len()
        {
            break records;
        }
        assert!(
            Instant::now() < deadline,
            "diagnostic batches were not persisted"
        );
        thread::sleep(Duration::from_millis(10));
    };

    let errors = records
        .iter()
        .filter(|record| record["event"] == "tool_error")
        .collect::<Vec<_>>();
    assert!(records.iter().all(|record| record["schema_version"] == 1));
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
    let mut session = Session::start(directory.path());
    let command = "pwsh -NoProfile -Command private_argument C:/private/path";
    session.call(
        1,
        "bash",
        &json!({ "command": command, "timeout_ms": 0 }),
        true,
    );
    session.close();

    let records = records(directory.path());
    let event = records
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
    let mut session = Session::start(&blocked);
    session.call_invalid_read(1);
    session.close();
}

#[test]
fn successful_default_mode_call_does_not_create_jsonl() {
    let directory = tempfile::tempdir().expect("log directory");
    let mut session = Session::start(directory.path());
    session.call_read(1, &json!("src/main.rs"), false);
    session.close();
    assert!(jsonl_paths(directory.path()).is_empty());
}

#[test]
fn all_mode_persists_modern_discovery_and_tool_list_metadata() {
    let directory = tempfile::tempdir().expect("log directory");
    let mut session = Session::start_with_mode(directory.path(), "all");

    session.send(&modern_request(1, "server/discover"));
    assert_eq!(session.receive()["id"], 1);
    session.send(&modern_request(2, "tools/list"));
    assert_eq!(session.receive()["id"], 2);
    session.close();

    let records = records(directory.path());
    let discover = records
        .iter()
        .find(|record| record["event"] == "discover")
        .expect("discover event");
    assert_eq!(discover["protocol"], "2026-07-28");
    assert_eq!(discover["client_name"], "diagnostics-modern");
    assert_eq!(discover["client_version"], "1.0.0");

    let tools_list = records
        .iter()
        .find(|record| record["event"] == "tools_list")
        .expect("tools list event");
    assert_eq!(tools_list["protocol"], "2026-07-28");
    assert_eq!(tools_list["tool_count"], 5);
    assert_eq!(tools_list["toolset"], "read,grep,glob,run_program,bash");
    assert_eq!(tools_list["has_cursor"], false);
    assert_eq!(tools_list["cache_ttl_ms"], 300_000);
    assert_eq!(tools_list["cache_scope"], "private");
    let request_id = tools_list["request_id"]
        .as_str()
        .expect("opaque request ID");
    assert!(uuid::Uuid::parse_str(request_id).is_ok());
    let sent = records
        .iter()
        .find(|record| record["event"] == "tools_list_sent")
        .expect("tools list sent event");
    assert_eq!(sent["request_id"], request_id);
    assert!(records.iter().any(|record| {
        record["event"] == "server_stop"
            && record["outcome"] == "success"
            && record["reason"] == "transport_closed"
    }));
}

#[test]
fn all_mode_persists_legacy_initialization_and_tool_list_metadata() {
    let directory = tempfile::tempdir().expect("log directory");
    let mut session = Session::start_with_mode(directory.path(), "all");

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

    let records = records(directory.path());
    let initialize = records
        .iter()
        .find(|record| record["event"] == "initialize")
        .expect("initialize event");
    assert_eq!(initialize["protocol"], "2025-11-25");
    assert_eq!(initialize["client_name"], "diagnostics-legacy");
    assert_eq!(initialize["client_version"], "1.0.0");
    assert!(
        records
            .iter()
            .any(|record| record["event"] == "initialized")
    );

    let tools_list = records
        .iter()
        .find(|record| record["event"] == "tools_list")
        .expect("tools list event");
    assert_eq!(tools_list["protocol"], "2025-11-25");
    assert_eq!(tools_list["client_name"], "diagnostics-legacy");
    assert_eq!(tools_list["tool_count"], 5);
    let request_id = tools_list["request_id"]
        .as_str()
        .expect("opaque request ID");
    assert_eq!(
        records
            .iter()
            .find(|record| record["event"] == "tools_list_sent")
            .expect("tools list sent event")["request_id"],
        request_id
    );
    assert!(records.iter().any(|record| {
        record["event"] == "server_stop"
            && record["outcome"] == "success"
            && record["reason"] == "transport_closed"
    }));
}

#[test]
fn tools_list_delivery_uses_an_opaque_correlation_id() {
    const SECRET_ID: &str = "private-client-request-id";

    let directory = tempfile::tempdir().expect("log directory");
    let mut session = Session::start_with_mode(directory.path(), "all");
    let mut request = modern_request(1, "tools/list");
    request["id"] = json!(SECRET_ID);
    session.send(&request);
    assert_eq!(session.receive()["id"], SECRET_ID);
    session.close();

    let records = records(directory.path());
    let serialized = serde_json::to_string(&records).expect("serialize records");
    assert!(!serialized.contains(SECRET_ID));
    let tools_list = records
        .iter()
        .find(|record| record["event"] == "tools_list")
        .expect("tools list event");
    let request_id = tools_list["request_id"]
        .as_str()
        .expect("opaque request ID");
    assert!(uuid::Uuid::parse_str(request_id).is_ok());
    assert_eq!(
        records
            .iter()
            .find(|record| record["event"] == "tools_list_sent")
            .expect("tools list sent event")["request_id"],
        request_id
    );
}

#[test]
fn tools_list_stdout_failure_is_correlated_and_stops_the_server() {
    const SECRET_ID: &str = "private-failed-request-id";

    let directory = tempfile::tempdir().expect("log directory");
    let mut session = Session::start_with_mode(directory.path(), "errors");
    session.close_stdout();
    let mut request = modern_request(1, "tools/list");
    request["id"] = json!(SECRET_ID);
    session.send(&request);
    let status = session.wait_for_exit();
    assert!(!status.success());

    let records = records(directory.path());
    let serialized = serde_json::to_string(&records).expect("serialize records");
    assert!(!serialized.contains(SECRET_ID));
    let tools_list = records
        .iter()
        .find(|record| record["event"] == "tools_list")
        .expect("tools list event");
    assert_eq!(tools_list["context"], true);
    let request_id = tools_list["request_id"]
        .as_str()
        .expect("opaque request ID");
    let write_error = records
        .iter()
        .find(|record| record["event"] == "stdout_write_error")
        .expect("stdout write error");
    assert_eq!(write_error["request_id"], request_id);
    assert_eq!(write_error["outcome"], "error");
    assert!(records.iter().any(|record| {
        record["event"] == "server_stop"
            && record["outcome"] == "error"
            && record["error_class"] == "transport"
    }));
}

#[test]
fn errors_mode_keeps_successful_handshakes_in_memory_until_an_error() {
    let directory = tempfile::tempdir().expect("log directory");
    let mut session = Session::start(directory.path());

    session.send(&modern_request(1, "server/discover"));
    assert_eq!(session.receive()["id"], 1);
    session.send(&modern_request(2, "tools/list"));
    assert_eq!(session.receive()["id"], 2);
    session.close();
    assert!(jsonl_paths(directory.path()).is_empty());

    let mut session = Session::start(directory.path());
    session.send(&modern_request(3, "server/discover"));
    assert_eq!(session.receive()["id"], 3);
    session.send(&modern_request(4, "tools/list"));
    assert_eq!(session.receive()["id"], 4);
    session.call_invalid_read(5);
    session.close();

    let records = records(directory.path());
    assert!(
        records
            .iter()
            .any(|record| record["event"] == "discover" && record["context"] == true)
    );
    assert!(
        records
            .iter()
            .any(|record| record["event"] == "tools_list" && record["context"] == true)
    );
    assert!(records.iter().any(|record| record["event"] == "tool_error"));
}

#[test]
fn oversized_frame_logs_a_safe_rejection_record() {
    const SECRET: &str = "diagnostics-stdio-secret";

    let directory = tempfile::tempdir().expect("log directory");
    let mut session = Session::start(directory.path());
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

    let records = records(directory.path());
    let rejection = records
        .iter()
        .find(|record| record["event"] == "mcp_frame_rejected")
        .expect("frame rejection event");
    assert_eq!(rejection["reason"], "frame_too_large");
    assert_eq!(rejection["frame_limit_bytes"], 8 * 1024 * 1024);
    assert!(records.iter().any(|record| {
        record["event"] == "server_stop"
            && record["outcome"] == "error"
            && record["error_class"] == "transport"
    }));
    assert!(
        !serde_json::to_string(&records)
            .expect("serialize records")
            .contains(SECRET)
    );
}

#[test]
fn missing_modern_metadata_logs_only_safe_framework_classification() {
    const SECRET: &str = "diagnostics-metadata-secret";

    let directory = tempfile::tempdir().expect("log directory");
    let mut session = Session::start(directory.path());
    session.send(&modern_request(1, "server/discover"));
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

    let records = records(directory.path());
    let framework_error = records
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
        !serde_json::to_string(&records)
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
        .join(format!("codexshim-{today}.0001.jsonl"));
    let expired = directory.path().join(format!("codexshim-{old}.0001.jsonl"));
    fs::write(&active, b"{}\n").expect("active log");
    fs::write(&expired, b"{}\n").expect("expired log");

    let status = Command::new(env!("CARGO_BIN_EXE_codexshim"))
        .args(["logs", "status"])
        .env("CODEXSHIM_LOG_MODE", "errors")
        .env("CODEXSHIM_LOG_DIR", directory.path())
        .output()
        .expect("status");
    assert!(status.status.success());
    let status_output = String::from_utf8(status.stdout).expect("status UTF-8");
    assert!(status_output.contains("JSONL files: 2"));
    assert!(status_output.contains("retention days: 30"));
    assert!(status_output.contains("recorded dropped records: 0"));

    let purge = Command::new(env!("CARGO_BIN_EXE_codexshim"))
        .args(["logs", "purge"])
        .env("CODEXSHIM_LOG_MODE", "errors")
        .env("CODEXSHIM_LOG_DIR", directory.path())
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
