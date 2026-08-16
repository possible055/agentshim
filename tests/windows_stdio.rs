#![cfg(windows)]

use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};

use serde_json::{Value, json};

struct Session {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Session {
    fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_agentshim"))
            .arg("serve")
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start agentshim");
        Self {
            stdin: child.stdin.take().expect("child stdin"),
            stdout: BufReader::new(child.stdout.take().expect("child stdout")),
            child,
        }
    }

    fn send(&mut self, message: &Value) {
        serde_json::to_writer(&mut self.stdin, message).expect("write request");
        self.stdin.write_all(b"\n").expect("write newline");
        self.stdin.flush().expect("flush request");
    }

    fn receive(&mut self) -> Value {
        let mut line = String::new();
        assert_ne!(
            self.stdout.read_line(&mut line).expect("read response"),
            0,
            "server closed stdout before responding"
        );
        serde_json::from_str(&line).expect("stdout line must be JSON")
    }

    fn close(mut self) {
        drop(self.stdin);
        assert!(self.child.wait().expect("wait for server").success());
    }
}

fn request(id: u64, method: &str, mut params: Value) -> Value {
    params.as_object_mut().expect("request parameters").insert(
        "_meta".to_owned(),
        json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientInfo": {
                "name": "agentshim-windows-wire-test",
                "version": "1.0.0"
            },
            "io.modelcontextprotocol/clientCapabilities": {}
        }),
    );
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params
    })
}

#[test]
fn cargo_multicall_proxy_reports_identity_on_nonzero_exit() {
    let mut session = Session::start();
    session.send(&request(1, "server/discover", json!({})));
    assert_eq!(session.receive()["id"], 1);
    session.send(&request(
        2,
        "tools/call",
        json!({
            "name": "run_program",
            "arguments": {
                "program": "cargo",
                "args": ["agentshim-definitely-not-a-command"],
                "cwd": env!("CARGO_MANIFEST_DIR"),
                "timeout_ms": 30_000
            }
        }),
    ));

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
            .file_stem()
            .and_then(|name| name.to_str()),
        Some("cargo")
    );
    assert!(output.contains("Launcher: native"));
    assert!(output.contains("Exit code: 101"));
    session.close();
}
