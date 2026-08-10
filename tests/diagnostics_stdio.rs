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
    stdout: BufReader<ChildStdout>,
}

impl Session {
    fn start(log_directory: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_codexshim"))
            .arg("serve")
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .env("CODEXSHIM_LOG_MODE", "errors")
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
            stdout,
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
        let stdin = self.stdin.as_mut().expect("stdin open");
        serde_json::to_writer(&mut *stdin, &request).expect("request");
        stdin.write_all(b"\n").expect("newline");
        stdin.flush().expect("flush");

        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("response");
        let response: Value = serde_json::from_str(&line).expect("JSON-RPC response");
        assert_eq!(response["id"], id);
        assert_eq!(response["result"]["isError"], expected_error);
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
