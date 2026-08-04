use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde_json::{Map, Value, json};

struct Session {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    transcript: Vec<String>,
}

impl Session {
    fn start() -> Self {
        Self::start_with_compatibility(None)
    }

    fn start_legacy() -> Self {
        Self::start_with_compatibility(Some("legacy"))
    }

    fn start_with_compatibility(compatibility: Option<&str>) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_codexshim"));
        command
            .arg("serve")
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .env_remove("CODEXSHIM_MCP_COMPATIBILITY")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(compatibility) = compatibility {
            command.env("CODEXSHIM_MCP_COMPATIBILITY", compatibility);
        }
        let mut child = command.spawn().expect("start codexshim");
        let stdin = child.stdin.take().expect("child stdin");
        let stdout = BufReader::new(child.stdout.take().expect("child stdout"));
        Self {
            child,
            stdin,
            stdout,
            transcript: Vec::new(),
        }
    }

    fn send(&mut self, message: &Value) {
        serde_json::to_writer(&mut self.stdin, message).expect("write request");
        self.stdin.write_all(b"\n").expect("write newline");
        self.stdin.flush().expect("flush request");
    }

    fn receive(&mut self) -> Value {
        let mut line = String::new();
        let bytes = self.stdout.read_line(&mut line).expect("read response");
        assert_ne!(bytes, 0, "server closed stdout before responding");
        let value = serde_json::from_str(&line).expect("stdout line must be JSON");
        self.transcript.push(line);
        value
    }

    fn close(mut self) {
        drop(self.stdin);
        let deadline = Instant::now() + Duration::from_secs(3);
        let status = loop {
            if let Some(status) = self.child.try_wait().expect("poll server") {
                break status;
            }
            if Instant::now() >= deadline {
                self.child.kill().expect("kill hung server");
                panic!("server did not exit promptly after stdin EOF");
            }
            thread::sleep(Duration::from_millis(10));
        };
        assert!(status.success(), "server exited with {status}");
        for line in self.transcript {
            serde_json::from_str::<Value>(&line).expect("transcript must remain parseable");
        }
    }
}

fn modern_meta() -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": {
            "name": "codexshim-wire-test",
            "version": "1.0.0"
        },
        "io.modelcontextprotocol/clientCapabilities": {}
    })
}

fn modern_request(id: u64, method: &str, mut params: Map<String, Value>) -> Value {
    params.insert("_meta".to_owned(), modern_meta());
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params
    })
}

fn empty_params() -> Map<String, Value> {
    Map::new()
}

fn assert_version_process_call(session: &mut Session, id: u64) {
    let mut process = empty_params();
    process.insert("name".to_owned(), json!("run_process"));
    process.insert(
        "arguments".to_owned(),
        json!({
            "program": env!("CARGO_BIN_EXE_codexshim"),
            "args": ["--version"],
            "timeout_ms": 5000
        }),
    );
    session.send(&modern_request(id, "tools/call", process));
    let process_result = session.receive();
    assert_eq!(process_result["id"], id);
    assert_eq!(process_result["result"]["isError"], false);
    let process_text = process_result["result"]["content"][0]["text"]
        .as_str()
        .expect("process text");
    assert!(process_text.contains("codexshim 0.1.0"));
    assert!(process_text.contains("Launcher: native"));
    assert!(process_text.contains("Exit code: 0"));
}

#[cfg(unix)]
fn start_process_tree(session: &mut Session, id: u64, pid_file: &std::path::Path) {
    let mut call = empty_params();
    call.insert("name".to_owned(), json!("run_process"));
    call.insert(
        "arguments".to_owned(),
        json!({
            "program": "/bin/sh",
            "args": ["-c", format!("sleep 30 & echo $! > '{}'; wait", pid_file.display())],
            "timeout_ms": 5000
        }),
    );
    session.send(&modern_request(id, "tools/call", call));
    let deadline = Instant::now() + Duration::from_secs(2);
    while !pid_file.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(pid_file.exists(), "process fixture did not start");
}

#[cfg(unix)]
fn assert_process_gone(pid_file: &std::path::Path) {
    let pid = std::fs::read_to_string(pid_file)
        .expect("descendant pid")
        .trim()
        .parse::<i32>()
        .expect("pid integer");
    let deadline = Instant::now() + Duration::from_secs(2);
    while unsafe { libc::kill(pid, 0) } == 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}

#[test]
fn modern_lifecycle_call_cancellation_and_eof_shutdown() {
    let mut session = Session::start();

    session.send(&modern_request(1, "server/discover", empty_params()));
    let discover = session.receive();
    assert_eq!(discover["id"], 1);
    assert_eq!(
        discover["result"]["supportedVersions"],
        json!(["2026-07-28"])
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
        ["read", "grep", "glob", "run_process"]
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
    assert!(read_text.contains("Path: "));
    assert!(read_text.contains("1\tuse std::"));
    assert!(read_text.ends_with("Complete."));

    let mut cancelled_call = empty_params();
    cancelled_call.insert("name".to_owned(), json!("grep"));
    cancelled_call.insert("arguments".to_owned(), json!({ "pattern": "never-return" }));
    session.send(&modern_request(4, "tools/call", cancelled_call));
    session.send(&json!({
        "jsonrpc": "2.0",
        "method": "notifications/cancelled",
        "params": { "requestId": 4, "reason": "wire test" }
    }));
    session.send(&modern_request(5, "tools/list", empty_params()));
    let after_cancel = session.receive();
    assert_eq!(
        after_cancel["id"], 5,
        "cancelled request emitted a response"
    );

    let mut invalid_read = empty_params();
    invalid_read.insert("name".to_owned(), json!("read"));
    invalid_read.insert(
        "arguments".to_owned(),
        json!({ "path": "src/main.rs", "unexpected": true }),
    );
    session.send(&modern_request(6, "tools/call", invalid_read));
    let validation = session.receive();
    assert_eq!(validation["id"], 6);
    assert_eq!(validation["result"]["isError"], true);

    let mut grep = empty_params();
    grep.insert("name".to_owned(), json!("grep"));
    grep.insert(
        "arguments".to_owned(),
        json!({
            "pattern": "CodexShim",
            "path": "src",
            "glob": "src/**/*.rs",
            "fixed_strings": true,
            "limit": 2
        }),
    );
    session.send(&modern_request(7, "tools/call", grep));
    let grep_result = session.receive();
    assert_eq!(grep_result["id"], 7);
    assert_eq!(grep_result["result"]["isError"], false);
    assert!(
        grep_result["result"]["content"][0]["text"]
            .as_str()
            .expect("grep text")
            .contains("CodexShim")
    );

    let mut glob = empty_params();
    glob.insert("name".to_owned(), json!("glob"));
    glob.insert(
        "arguments".to_owned(),
        json!({ "pattern": "src/**/*.rs", "limit": 2 }),
    );
    session.send(&modern_request(8, "tools/call", glob));
    let glob_result = session.receive();
    assert_eq!(glob_result["id"], 8);
    assert_eq!(glob_result["result"]["isError"], false);
    assert!(
        glob_result["result"]["content"][0]["text"]
            .as_str()
            .expect("glob text")
            .contains("/src/")
    );

    assert_version_process_call(&mut session, 9);

    session.close();
}

#[test]
fn missing_modern_metadata_is_rejected_without_corrupting_stdio() {
    let mut session = Session::start();
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
fn strict_compatibility_rejects_legacy_initialize() {
    let mut session = Session::start();
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
    let response = session.receive();
    assert_eq!(response["id"], 1);
    assert_eq!(response["error"]["code"], -32601);

    drop(session.stdin);
    let status = session.child.wait().expect("wait for rejected server");
    assert!(!status.success());
}

#[test]
fn legacy_compatibility_rejects_unlisted_protocol_versions() {
    let mut session = Session::start_legacy();
    session.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": { "name": "legacy-test", "version": "1.0.0" }
        }
    }));
    let response = session.receive();
    assert_eq!(response["id"], 1);
    assert_eq!(response["error"]["code"], -32601);

    drop(session.stdin);
    let status = session.child.wait().expect("wait for rejected server");
    assert!(!status.success());
}

#[test]
fn explicit_legacy_compatibility_uses_native_initialize_lifecycle() {
    let mut session = Session::start_legacy();
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
        ["read", "grep", "glob", "run_process"]
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
            .contains("1\tuse std::")
    );
    session.close();
}

#[test]
fn explicit_legacy_compatibility_advertises_modern_first() {
    let mut session = Session::start_legacy();
    session.send(&modern_request(1, "server/discover", empty_params()));
    let discover = session.receive();
    assert_eq!(discover["id"], 1);
    assert_eq!(
        discover["result"]["supportedVersions"],
        json!(["2026-07-28", "2025-06-18"])
    );
    session.close();
}

#[test]
fn eight_parallel_read_only_calls_complete_without_protocol_corruption() {
    let mut session = Session::start();
    session.send(&modern_request(1, "server/discover", empty_params()));
    assert_eq!(session.receive()["id"], 1);

    for id in 10..18 {
        let mut call = empty_params();
        call.insert("name".to_owned(), json!("glob"));
        call.insert(
            "arguments".to_owned(),
            json!({ "pattern": "src/**/*.rs", "limit": 10 }),
        );
        session.send(&modern_request(id, "tools/call", call));
    }
    let mut ids = Vec::new();
    for _ in 0..8 {
        let response = session.receive();
        assert_eq!(response["result"]["isError"], false);
        ids.push(response["id"].as_u64().expect("numeric id"));
    }
    ids.sort_unstable();
    assert_eq!(ids, (10..18).collect::<Vec<_>>());
    session.close();
}

#[test]
fn eight_parallel_process_calls_respect_admission_without_protocol_corruption() {
    let mut session = Session::start();
    session.send(&modern_request(1, "server/discover", empty_params()));
    assert_eq!(session.receive()["id"], 1);

    for id in 20..28 {
        let mut call = empty_params();
        call.insert("name".to_owned(), json!("run_process"));
        call.insert(
            "arguments".to_owned(),
            json!({
                "program": env!("CARGO_BIN_EXE_codexshim"),
                "args": ["--version"],
                "timeout_ms": 5000
            }),
        );
        session.send(&modern_request(id, "tools/call", call));
    }
    let mut ids = Vec::new();
    for _ in 0..8 {
        let response = session.receive();
        assert_eq!(response["result"]["isError"], false);
        ids.push(response["id"].as_u64().expect("numeric id"));
    }
    ids.sort_unstable();
    assert_eq!(ids, (20..28).collect::<Vec<_>>());
    session.close();
}

#[cfg(unix)]
#[test]
fn run_process_preserves_cargo_multicall_proxy_identity() {
    let mut session = Session::start();
    session.send(&modern_request(1, "server/discover", empty_params()));
    assert_eq!(session.receive()["id"], 1);

    let mut process = empty_params();
    process.insert("name".to_owned(), json!("run_process"));
    process.insert(
        "arguments".to_owned(),
        json!({
            "program": "cargo",
            "args": [
                "test",
                "--locked",
                "--test",
                "modern_stdio",
                "explicit_legacy_compatibility_uses_native_initialize_lifecycle",
                "--",
                "--nocapture"
            ],
            "cwd": env!("CARGO_MANIFEST_DIR"),
            "timeout_ms": 120_000
        }),
    );
    session.send(&modern_request(2, "tools/call", process));
    let response = session.receive();
    assert_eq!(response["id"], 2);
    assert_eq!(response["result"]["isError"], false);
    let output = response["result"]["content"][0]["text"]
        .as_str()
        .expect("process output");
    let resolved = output
        .lines()
        .find_map(|line| line.strip_prefix("Resolved program: "))
        .expect("resolved program line");
    assert_eq!(
        std::path::Path::new(resolved)
            .file_name()
            .and_then(|name| name.to_str()),
        Some("cargo")
    );
    assert!(
        output
            .contains("test explicit_legacy_compatibility_uses_native_initialize_lifecycle ... ok")
    );
    assert!(output.contains("1 passed; 0 failed"));
    assert!(output.contains("Exit code: 0"));
    session.close();
}

#[cfg(unix)]
#[test]
fn stdio_eof_terminates_running_process_tree() {
    let fixture = tempfile::tempdir().expect("fixture");
    let pid_file = fixture.path().join("descendant.pid");
    let mut session = Session::start();
    session.send(&modern_request(1, "server/discover", empty_params()));
    assert_eq!(session.receive()["id"], 1);

    start_process_tree(&mut session, 2, &pid_file);
    session.close();
    assert_process_gone(&pid_file);
}

#[cfg(unix)]
#[test]
fn mcp_cancellation_terminates_running_process_tree() {
    let fixture = tempfile::tempdir().expect("fixture");
    let pid_file = fixture.path().join("descendant.pid");
    let mut session = Session::start();
    session.send(&modern_request(1, "server/discover", empty_params()));
    assert_eq!(session.receive()["id"], 1);

    start_process_tree(&mut session, 2, &pid_file);
    session.send(&json!({
        "jsonrpc": "2.0",
        "method": "notifications/cancelled",
        "params": { "requestId": 2, "reason": "process cancellation test" }
    }));
    session.send(&modern_request(3, "tools/list", empty_params()));
    let response = session.receive();
    assert_eq!(
        response["id"], 3,
        "cancelled process request emitted a response"
    );
    assert_process_gone(&pid_file);
    session.close();
}
