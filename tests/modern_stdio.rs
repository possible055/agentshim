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

#[test]
fn modern_lifecycle_serves_a_tool_call_and_shuts_down_at_eof() {
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
