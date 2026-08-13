use super::*;

pub(super) struct Session {
    pub(super) child: Child,
    pub(super) stdin: Option<ChildStdin>,
    pub(super) stdout: BufReader<ChildStdout>,
    transcript: Vec<String>,
}

impl Session {
    pub(super) fn start() -> Self {
        Self::start_with_options(None, None, None)
    }

    pub(super) fn start_strict() -> Self {
        Self::start_with_options(Some("strict"), None, None)
    }

    pub(super) fn start_unrestricted() -> Self {
        Self::start_with_options(None, Some("unrestricted"), None)
    }

    pub(super) fn start_with_process_calls(process_calls: usize) -> Self {
        Self::start_with_options(None, None, Some(process_calls))
    }

    pub(super) fn start_at(root: &std::path::Path) -> Self {
        Self::spawn(Self::base_command(root))
    }

    pub(super) fn start_with_options(
        compatibility: Option<&str>,
        read_scope: Option<&str>,
        process_calls: Option<usize>,
    ) -> Self {
        let mut command = Self::base_command(std::path::Path::new(env!("CARGO_MANIFEST_DIR")));
        if let Some(read_scope) = read_scope {
            command.args(["--read-scope", read_scope]);
        }
        if let Some(compatibility) = compatibility {
            command.env("CODEXSHIM_MCP_COMPATIBILITY", compatibility);
        }
        if let Some(process_calls) = process_calls {
            command.env("CODEXSHIM_PROCESS_CALLS", process_calls.to_string());
        }
        Self::spawn(command)
    }

    pub(super) fn start_for_bash(
        root: &std::path::Path,
        detached_calls: usize,
        bash_override: Option<&std::path::Path>,
    ) -> Self {
        let mut command = Self::base_command(root);
        command.env("CODEXSHIM_DETACHED_CALLS", detached_calls.to_string());
        if let Some(bash_override) = bash_override {
            command.env("CODEXSHIM_BASH", bash_override);
        }
        Self::spawn(command)
    }

    pub(super) fn base_command(root: &std::path::Path) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_codexshim"));
        command
            .arg("serve")
            .current_dir(root)
            .env_remove("CODEXSHIM_MCP_COMPATIBILITY")
            .env_remove("CODEXSHIM_PROCESS_CALLS")
            .env_remove("CODEXSHIM_DETACHED_CALLS")
            .env_remove("CODEXSHIM_BASH")
            .env_remove("CODEXSHIM_BURST_TOKENS")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        command
    }

    pub(super) fn spawn(mut command: Command) -> Self {
        let mut child = command.spawn().expect("start codexshim");
        let stdin = child.stdin.take().expect("child stdin");
        let stdout = BufReader::new(child.stdout.take().expect("child stdout"));
        Self {
            child,
            stdin: Some(stdin),
            stdout,
            transcript: Vec::new(),
        }
    }

    pub(super) fn send(&mut self, message: &Value) {
        let stdin = self.stdin.as_mut().expect("server stdin");
        serde_json::to_writer(&mut *stdin, message).expect("write request");
        stdin.write_all(b"\n").expect("write newline");
        stdin.flush().expect("flush request");
    }

    pub(super) fn receive(&mut self) -> Value {
        let mut line = String::new();
        let bytes = self.stdout.read_line(&mut line).expect("read response");
        assert_ne!(bytes, 0, "server closed stdout before responding");
        let value = serde_json::from_str(&line).expect("stdout line must be JSON");
        self.transcript.push(line);
        value
    }

    pub(super) fn close(mut self) {
        self.stdin.take();
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
        for line in &self.transcript {
            serde_json::from_str::<Value>(line).expect("transcript must remain parseable");
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

pub(super) fn modern_meta() -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": {
            "name": "codexshim-wire-test",
            "version": "1.0.0"
        },
        "io.modelcontextprotocol/clientCapabilities": {}
    })
}

pub(super) fn modern_request(id: u64, method: &str, mut params: Map<String, Value>) -> Value {
    params.insert("_meta".to_owned(), modern_meta());
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params
    })
}

pub(super) fn empty_params() -> Map<String, Value> {
    Map::new()
}

pub(super) fn call_tool(session: &mut Session, id: u64, name: &str, arguments: Value) -> Value {
    let mut call = empty_params();
    call.insert("name".to_owned(), json!(name));
    call.insert("arguments".to_owned(), arguments);
    session.send(&modern_request(id, "tools/call", call));
    session.receive()
}

pub(super) fn response_text(response: &Value) -> &str {
    response["result"]["content"][0]["text"]
        .as_str()
        .expect("tool response text")
}

pub(super) fn pdf_with_text() -> Vec<u8> {
    let mut pdf = b"%PDF-1.7\n".to_vec();
    let mut offsets = [0_usize; 6];
    let mut object = |id: usize, body: &[u8]| {
        offsets[id] = pdf.len();
        pdf.extend_from_slice(format!("{id} 0 obj\n").as_bytes());
        pdf.extend_from_slice(body);
        pdf.extend_from_slice(b"\nendobj\n");
    };
    object(1, b"<< /Type /Catalog /Pages 2 0 R >>");
    object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>");
    object(
        3,
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>",
    );
    let content = b"BT /F1 18 Tf 20 150 Td (PDF image block) Tj ET";
    let mut stream = format!("<< /Length {} >>\nstream\n", content.len()).into_bytes();
    stream.extend_from_slice(content);
    stream.extend_from_slice(b"\nendstream");
    object(4, &stream);
    object(
        5,
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>",
    );
    let xref = pdf.len();
    pdf.extend_from_slice(b"xref\n0 6\n0000000000 65535 f \n");
    for offset in offsets.iter().skip(1) {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(b"trailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n");
    pdf.extend_from_slice(format!("{xref}\n%%EOF\n").as_bytes());
    pdf
}
