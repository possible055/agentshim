use std::{
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};

use super::fixtures::{empty_params, modern_request};

#[derive(Debug, Clone)]
pub struct TestSessionBuilder {
    root: PathBuf,
    profile: Option<&'static str>,
    read_scope: Option<&'static str>,
    idle_timeout_secs: Option<u64>,
    log_mode: Option<&'static str>,
    log_dir: Option<PathBuf>,
    process_calls: Option<usize>,
    detached_calls: Option<usize>,
    burst_tokens: Option<usize>,
    output_bytes: Option<usize>,
    bash_override: Option<PathBuf>,
    env_vars: Vec<(&'static str, String)>,
}

impl Default for TestSessionBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl TestSessionBuilder {
    pub fn new() -> Self {
        Self {
            root: PathBuf::from(env!("CARGO_MANIFEST_DIR")),
            profile: None,
            read_scope: None,
            idle_timeout_secs: None,
            log_mode: None,
            log_dir: None,
            process_calls: None,
            detached_calls: None,
            burst_tokens: None,
            output_bytes: None,
            bash_override: None,
            env_vars: Vec::new(),
        }
    }

    pub fn root(mut self, path: impl AsRef<Path>) -> Self {
        self.root = path.as_ref().to_path_buf();
        self
    }

    pub fn profile(mut self, profile: &'static str) -> Self {
        self.profile = Some(profile);
        self
    }

    pub fn read_scope(mut self, scope: &'static str) -> Self {
        self.read_scope = Some(scope);
        self
    }

    pub fn idle_timeout_secs(mut self, secs: u64) -> Self {
        self.idle_timeout_secs = Some(secs);
        self
    }

    pub fn log_mode(mut self, mode: &'static str) -> Self {
        self.log_mode = Some(mode);
        self
    }

    pub fn log_dir(mut self, dir: impl AsRef<Path>) -> Self {
        self.log_dir = Some(dir.as_ref().to_path_buf());
        self
    }

    pub fn process_calls(mut self, calls: usize) -> Self {
        self.process_calls = Some(calls);
        self
    }

    pub fn detached_calls(mut self, calls: usize) -> Self {
        self.detached_calls = Some(calls);
        self
    }

    pub fn burst_tokens(mut self, tokens: usize) -> Self {
        self.burst_tokens = Some(tokens);
        self
    }

    pub fn output_bytes(mut self, bytes: usize) -> Self {
        self.output_bytes = Some(bytes);
        self
    }

    pub fn bash_override(mut self, bash: impl AsRef<Path>) -> Self {
        self.bash_override = Some(bash.as_ref().to_path_buf());
        self
    }

    pub fn env(mut self, key: &'static str, value: impl Into<String>) -> Self {
        self.env_vars.push((key, value.into()));
        self
    }

    pub fn build_command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_agentshim"));
        command.arg("serve");
        if let Some(profile) = self.profile {
            command.args(["--client-profile", profile]);
        }
        if let Some(read_scope) = self.read_scope {
            command.args(["--read-scope", read_scope]);
        }
        command.current_dir(&self.root);

        command
            .env_remove("AGENTSHIM_PROCESS_CALLS")
            .env_remove("AGENTSHIM_DETACHED_CALLS")
            .env_remove("AGENTSHIM_DETACHED_LOG_BYTES")
            .env_remove("AGENTSHIM_WINDOWS_ACTIVE_PROCESS_LIMIT")
            .env_remove("AGENTSHIM_WINDOWS_JOB_MEMORY_BYTES")
            .env_remove("AGENTSHIM_WINDOWS_PROCESS_MEMORY_BYTES")
            .env_remove("AGENTSHIM_BASH")
            .env_remove("AGENTSHIM_BURST_TOKENS")
            .env_remove("AGENTSHIM_IDLE_TIMEOUT");

        if let Some(timeout) = self.idle_timeout_secs {
            command.env("AGENTSHIM_IDLE_TIMEOUT", timeout.to_string());
        }
        if let Some(log_mode) = self.log_mode {
            command.env("AGENTSHIM_LOG_MODE", log_mode);
        }
        if let Some(ref log_dir) = self.log_dir {
            command.env("AGENTSHIM_LOG_DIR", log_dir);
        }
        if let Some(process_calls) = self.process_calls {
            command.env("AGENTSHIM_PROCESS_CALLS", process_calls.to_string());
        }
        if let Some(detached_calls) = self.detached_calls {
            command.env("AGENTSHIM_DETACHED_CALLS", detached_calls.to_string());
        }
        if let Some(burst_tokens) = self.burst_tokens {
            command.env("AGENTSHIM_BURST_TOKENS", burst_tokens.to_string());
        }
        if let Some(output_bytes) = self.output_bytes {
            command.env("AGENTSHIM_OUTPUT_BYTES", output_bytes.to_string());
        }
        if let Some(ref bash) = self.bash_override {
            command.env("AGENTSHIM_BASH", bash);
        }

        for (k, v) in &self.env_vars {
            command.env(k, v);
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        command
    }

    pub fn spawn(self) -> TestSession {
        let mut command = self.build_command();
        let mut child = command.spawn().expect("start agentshim");
        let stdin = child.stdin.take().expect("child stdin");
        let stdout = child.stdout.take().expect("child stdout");
        TestSession {
            child,
            stdin: Some(stdin),
            stdout: Some(BufReader::new(stdout)),
            transcript: Vec::new(),
        }
    }
}

pub struct TestSession {
    pub child: Child,
    pub stdin: Option<ChildStdin>,
    pub stdout: Option<BufReader<ChildStdout>>,
    pub transcript: Vec<String>,
}

impl TestSession {
    pub fn builder() -> TestSessionBuilder {
        TestSessionBuilder::new()
    }

    pub fn start() -> Self {
        Self::builder().spawn()
    }

    pub fn start_at(root: impl AsRef<Path>) -> Self {
        Self::builder().root(root).spawn()
    }

    pub fn start_unrestricted() -> Self {
        Self::builder().read_scope("unrestricted").spawn()
    }

    pub fn start_normal_at(root: impl AsRef<Path>) -> Self {
        Self::builder().root(root).read_scope("normal").spawn()
    }

    #[allow(dead_code)] // Used by the separately compiled opt-in resource_soak runner.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn send(&mut self, message: &Value) {
        let stdin = self.stdin.as_mut().expect("server stdin open");
        serde_json::to_writer(&mut *stdin, message).expect("write request");
        stdin.write_all(b"\n").expect("write newline");
        stdin.flush().expect("flush request");
    }

    pub fn receive(&mut self) -> Value {
        let mut line = String::new();
        let bytes = self
            .stdout
            .as_mut()
            .expect("server stdout open")
            .read_line(&mut line)
            .expect("read response");
        assert_ne!(bytes, 0, "server closed stdout before responding");
        let value = serde_json::from_str(&line).expect("stdout line must be JSON");
        self.transcript.push(line);
        value
    }

    pub fn handshake(&mut self) {
        self.send(&modern_request(1, "server/discover", empty_params()));
        assert_eq!(self.receive()["id"], 1);
    }

    pub fn call_tool(&mut self, id: u64, name: &str, arguments: Value) -> Value {
        let mut call = empty_params();
        call.insert("name".to_owned(), json!(name));
        call.insert("arguments".to_owned(), arguments);
        self.send(&modern_request(id, "tools/call", call));
        self.receive()
    }

    pub fn call(&mut self, id: u64, name: &str, arguments: &Value, expected_error: bool) {
        let response = self.call_tool(id, name, arguments.clone());
        assert_eq!(response["id"], id);
        assert_eq!(response["result"]["isError"], expected_error);
    }

    pub fn call_read(&mut self, id: u64, path: &Value, expected_error: bool) {
        self.call(id, "read", &json!({ "path": path }), expected_error);
    }

    pub fn call_invalid_read(&mut self, id: u64) {
        self.call_read(id, &json!(1), true);
    }

    pub fn close_stdout(&mut self) {
        self.stdout.take();
    }

    pub fn assert_alive_for(&mut self, duration: Duration) {
        let deadline = Instant::now() + duration;
        while Instant::now() < deadline {
            assert!(
                self.child.try_wait().expect("poll server").is_none(),
                "server exited prematurely"
            );
            thread::sleep(Duration::from_millis(25));
        }
    }

    pub fn wait_for_exit(&mut self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().expect("poll server") {
                return status;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                panic!("server did not exit before the deadline");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn shutdown(mut self) -> ExitStatus {
        self.stdin.take();
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = self.child.try_wait().expect("poll server") {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                panic!("server did not exit promptly after stdin EOF");
            }
            thread::sleep(Duration::from_millis(10));
        };
        for line in &self.transcript {
            serde_json::from_str::<Value>(line).expect("transcript must remain parseable");
        }
        status
    }

    pub fn close(self) {
        let status = self.shutdown();
        assert!(status.success(), "server exited with {status}");
    }
}

impl Drop for TestSession {
    fn drop(&mut self) {
        self.stdin.take();
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}
