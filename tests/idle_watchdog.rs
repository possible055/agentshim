use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};

struct Session {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Session {
    fn start(root: &std::path::Path, profile: &str, timeout_secs: u64) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_codexshim"));
        command
            .args(["serve", "--client-profile", profile])
            .current_dir(root)
            .env_remove("CODEXSHIM_IDLE_TIMEOUT")
            .env("CODEXSHIM_IDLE_TIMEOUT", timeout_secs.to_string())
            .env("CODEXSHIM_LOG_MODE", "off")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command.spawn().expect("start codexshim");
        let stdin = child.stdin.take().expect("child stdin");
        let stdout = BufReader::new(child.stdout.take().expect("child stdout"));
        Self {
            child,
            stdin,
            stdout,
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
        assert_ne!(bytes, 0, "server closed before responding");
        serde_json::from_str(&line).expect("response JSON")
    }

    fn handshake(&mut self) {
        self.send(&request(1, "server/discover", json!({})));
        assert_eq!(self.receive()["id"], 1);
    }

    fn assert_alive_for(&mut self, duration: Duration) {
        let deadline = Instant::now() + duration;
        while Instant::now() < deadline {
            assert!(
                self.child.try_wait().expect("poll server").is_none(),
                "server exited before the watchdog should fire"
            );
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn wait_for_exit(&mut self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().expect("poll server") {
                return status;
            }
            if Instant::now() >= deadline {
                self.child.kill().expect("kill hung server");
                panic!("server did not exit before the watchdog deadline");
            }
            thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn request(id: u64, method: &str, mut params: Value) -> Value {
    params
        .as_object_mut()
        .expect("request params object")
        .insert(
            "_meta".to_owned(),
            json!({
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientInfo": {
                    "name": "idle-watchdog-test",
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

fn doctor(profile: &str, timeout: Option<&str>) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_codexshim"));
    command
        .args(["doctor", "--client-profile", profile])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .env_remove("CODEXSHIM_IDLE_TIMEOUT")
        .env("CODEXSHIM_LOG_MODE", "off");
    if let Some(timeout) = timeout {
        command.env("CODEXSHIM_IDLE_TIMEOUT", timeout);
    }
    command.output().expect("run doctor")
}

#[test]
fn invalid_values_fail_startup_and_doctor_reports_profile_gating() {
    for value in ["0", "86401", "many", "-1"] {
        let output = doctor("codex", Some(value));
        assert!(!output.status.success(), "{value} must be rejected");
        assert!(String::from_utf8_lossy(&output.stderr).contains("CODEXSHIM_IDLE_TIMEOUT"));
    }

    let codex = doctor("codex", Some("7"));
    assert!(codex.status.success());
    assert!(String::from_utf8_lossy(&codex.stdout).contains("idle timeout: 7s"));

    let cursor = doctor("cursor", Some("7"));
    assert!(cursor.status.success());
    assert!(String::from_utf8_lossy(&cursor.stdout).contains("idle timeout: disabled"));

    let disabled = doctor("codex", None);
    assert!(disabled.status.success());
    assert!(String::from_utf8_lossy(&disabled.stdout).contains("idle timeout: disabled"));
}

#[test]
fn codex_profile_exits_cleanly_after_handshake_goes_idle() {
    let mut session = Session::start(std::path::Path::new(env!("CARGO_MANIFEST_DIR")), "codex", 1);
    session.handshake();

    let status = session.wait_for_exit(Duration::from_secs(10));

    assert!(status.success(), "server exited with {status}");
}

#[test]
fn cursor_profile_ignores_the_idle_timeout() {
    let mut session = Session::start(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")),
        "cursor",
        1,
    );
    session.handshake();

    session.assert_alive_for(Duration::from_secs(3));
}

#[test]
fn inbound_pings_keep_the_server_alive_until_they_stop() {
    let mut session = Session::start(std::path::Path::new(env!("CARGO_MANIFEST_DIR")), "codex", 2);
    session.handshake();

    for id in 2..=8 {
        thread::sleep(Duration::from_millis(500));
        session.send(&request(id, "ping", json!({})));
        assert_eq!(session.receive()["id"], id);
    }
    assert!(session.child.try_wait().expect("poll server").is_none());

    let status = session.wait_for_exit(Duration::from_secs(10));
    assert!(status.success(), "server exited with {status}");
}

#[test]
fn a_live_detached_tree_defers_idle_shutdown() {
    if codexshim::bash_report().is_err() {
        return;
    }
    let fixture = tempfile::tempdir().expect("fixture");
    let mut session = Session::start(fixture.path(), "codex", 1);
    session.handshake();
    session.send(&request(
        2,
        "tools/call",
        json!({
            "name": "bash",
            "arguments": {
                "command": "sleep 5",
                "detach": true,
                "log_path": "idle-detached.log"
            }
        }),
    ));
    let response = session.receive();
    assert_eq!(response["result"]["isError"], false, "{response}");

    session.assert_alive_for(Duration::from_secs(3));
    let status = session.wait_for_exit(Duration::from_secs(10));

    assert!(status.success(), "server exited with {status}");
}
