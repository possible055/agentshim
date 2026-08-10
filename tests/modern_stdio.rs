use std::{
    collections::BTreeSet,
    fmt::Write as _,
    fs,
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use base64::Engine as _;
use serde_json::{Map, Value, json};

const MAX_RECEIVE_FRAME_BYTES: usize = 8 * 1024 * 1024;

struct Session {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    transcript: Vec<String>,
}

impl Session {
    fn start() -> Self {
        Self::start_with_options(None, None, None)
    }

    fn start_strict() -> Self {
        Self::start_with_options(Some("strict"), None, None)
    }

    fn start_unrestricted() -> Self {
        Self::start_with_options(None, Some("unrestricted"), None)
    }

    fn start_with_process_calls(process_calls: usize) -> Self {
        Self::start_with_options(None, None, Some(process_calls))
    }

    fn start_at(root: &std::path::Path) -> Self {
        Self::spawn(Self::base_command(root))
    }

    fn start_with_options(
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

    fn start_for_bash(
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

    fn base_command(root: &std::path::Path) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_codexshim"));
        command
            .arg("serve")
            .current_dir(root)
            .env_remove("CODEXSHIM_MCP_COMPATIBILITY")
            .env_remove("CODEXSHIM_PROCESS_CALLS")
            .env_remove("CODEXSHIM_DETACHED_CALLS")
            .env_remove("CODEXSHIM_BASH")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        command
    }

    fn spawn(mut command: Command) -> Self {
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

    fn send(&mut self, message: &Value) {
        let stdin = self.stdin.as_mut().expect("server stdin");
        serde_json::to_writer(&mut *stdin, message).expect("write request");
        stdin.write_all(b"\n").expect("write newline");
        stdin.flush().expect("flush request");
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

fn call_tool(session: &mut Session, id: u64, name: &str, arguments: Value) -> Value {
    let mut call = empty_params();
    call.insert("name".to_owned(), json!(name));
    call.insert("arguments".to_owned(), arguments);
    session.send(&modern_request(id, "tools/call", call));
    session.receive()
}

fn response_text(response: &Value) -> &str {
    response["result"]["content"][0]["text"]
        .as_str()
        .expect("tool response text")
}

fn pdf_with_text() -> Vec<u8> {
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

#[test]
fn bash_toolchain_commands_work_over_real_stdio() {
    if codexshim::bash_report().is_err() {
        return;
    }
    let mut session = Session::start();
    session.send(&modern_request(1, "server/discover", empty_params()));
    assert_eq!(session.receive()["id"], 1);

    let response = call_tool(
        &mut session,
        2,
        "bash",
        json!({
            "command": "sleep 0.05; printf 'needle\\n' | grep needle | sed 's/needle/toolchain-ok/'; locale >/dev/null"
        }),
    );

    assert_eq!(response["result"]["isError"], false);
    assert!(response_text(&response).contains("toolchain-ok"));
    assert!(response_text(&response).contains("Exit code: 0"));
    session.close();
}

#[test]
fn detached_roster_saturation_fails_before_blocking_scheduling_over_stdio() {
    if codexshim::bash_report().is_err() {
        return;
    }
    let fixture = tempfile::tempdir().expect("fixture");
    let mut session = Session::start_for_bash(fixture.path(), 1, None);
    session.send(&modern_request(1, "server/discover", empty_params()));
    assert_eq!(session.receive()["id"], 1);
    let first = call_tool(
        &mut session,
        2,
        "bash",
        json!({
            "command": "sleep 30",
            "detach": true,
            "log_path": "first.log"
        }),
    );
    assert_eq!(first["result"]["isError"], false);

    let started = Instant::now();
    let second = call_tool(
        &mut session,
        3,
        "bash",
        json!({
            "command": "sleep 30",
            "detach": true,
            "log_path": "second.log"
        }),
    );

    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(second["result"]["isError"], true);
    assert_eq!(
        second["result"]["structuredContent"]["error"]["code"],
        "resource_busy"
    );
    assert!(response_text(&second).contains("first.log"));
    assert!(response_text(&second).contains("pid "));
    session.close();
}

#[test]
fn missing_bash_is_non_retryable_over_real_stdio() {
    let fixture = tempfile::tempdir().expect("fixture");
    let missing = fixture.path().join("missing-bash");
    let mut session = Session::start_for_bash(fixture.path(), 1, Some(&missing));
    session.send(&modern_request(1, "server/discover", empty_params()));
    assert_eq!(session.receive()["id"], 1);

    let response = call_tool(&mut session, 2, "bash", json!({ "command": "true" }));

    assert_eq!(response["result"]["isError"], true);
    assert_eq!(
        response["result"]["structuredContent"]["error"]["code"],
        "io"
    );
    assert_eq!(
        response["result"]["structuredContent"]["error"]["retryable"],
        false
    );
    assert!(response_text(&response).contains("CODEXSHIM_BASH"));
    session.close();
}

#[test]
fn modern_lifecycle_serves_a_tool_call_and_shuts_down_at_eof() {
    let mut session = Session::start();

    session.send(&modern_request(1, "server/discover", empty_params()));
    let discover = session.receive();
    assert_eq!(discover["id"], 1);
    assert_eq!(
        discover["result"]["supportedVersions"],
        json!(["2026-07-28", "2025-06-18"])
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
        ["read", "grep", "glob", "run_program", "bash"]
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
    assert!(read_text.contains("1\tinclude!(\"cli/transport.rs\");"));
    assert!(read_text.ends_with("Complete."));
    assert!(
        response["result"].get("structuredContent").is_none(),
        "read success must not emit structured content"
    );

    session.close();
}

#[test]
fn pdf_image_read_returns_an_image_content_block_over_real_stdio() {
    let fixture = tempfile::tempdir().expect("fixture");
    fs::write(fixture.path().join("document.pdf"), pdf_with_text()).expect("PDF fixture");
    let mut session = Session::start_at(fixture.path());
    session.send(&modern_request(1, "server/discover", empty_params()));
    assert_eq!(session.receive()["id"], 1);

    let response = call_tool(
        &mut session,
        2,
        "read",
        json!({
            "path": "document.pdf",
            "pdf_mode": "image",
            "pages": "1"
        }),
    );

    assert_eq!(response["result"]["isError"], false);
    let content = response["result"]["content"]
        .as_array()
        .expect("content blocks");
    assert_eq!(content.len(), 2);
    assert_eq!(content[1]["type"], "image");
    assert_eq!(content[1]["mimeType"], "image/png");
    let encoded = content[1]["data"].as_str().expect("base64 image data");
    let png = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .expect("valid base64");
    assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
    session.close();
}

fn pdf_full_page_image() -> Vec<u8> {
    let pixels = vec![0x80_u8; 80 * 80 * 3];
    let mut image = format!(
        "<< /Type /XObject /Subtype /Image /Width 80 /Height 80 /ColorSpace /DeviceRGB \
         /BitsPerComponent 8 /Length {} >>\nstream\n",
        pixels.len()
    )
    .into_bytes();
    image.extend_from_slice(&pixels);
    image.extend_from_slice(b"\nendstream");
    let operations = b"q 200 0 0 200 0 0 cm /Im0 Do Q";
    let mut content = format!("<< /Length {} >>\nstream\n", operations.len()).into_bytes();
    content.extend_from_slice(operations);
    content.extend_from_slice(b"\nendstream");

    let bodies: [Vec<u8>; 5] = [
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources \
          << /XObject << /Im0 4 0 R >> >> /Contents 5 0 R >>"
            .to_vec(),
        image,
        content,
    ];
    let mut pdf = b"%PDF-1.7\n".to_vec();
    let mut offsets = vec![0_usize; bodies.len() + 1];
    for (index, body) in bodies.iter().enumerate() {
        let id = index + 1;
        offsets[id] = pdf.len();
        pdf.extend_from_slice(format!("{id} 0 obj\n").as_bytes());
        pdf.extend_from_slice(body);
        pdf.extend_from_slice(b"\nendobj\n");
    }
    let size = bodies.len() + 1;
    let xref = pdf.len();
    pdf.extend_from_slice(format!("xref\n0 {size}\n0000000000 65535 f \n").as_bytes());
    for offset in offsets.iter().skip(1) {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!("trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n").as_bytes(),
    );
    pdf.extend_from_slice(format!("{xref}\n%%EOF\n").as_bytes());
    pdf
}

/// `pdf_image_required` is not retryable with the same parameters, so the parameters that
/// would work have to travel with the error rather than only in its message.
#[test]
fn pdf_image_required_carries_structured_retry_parameters_over_real_stdio() {
    let fixture = tempfile::tempdir().expect("fixture");
    fs::write(fixture.path().join("scan.pdf"), pdf_full_page_image()).expect("PDF fixture");
    let mut session = Session::start_at(fixture.path());
    session.send(&modern_request(1, "server/discover", empty_params()));
    assert_eq!(session.receive()["id"], 1);

    let response = call_tool(&mut session, 2, "read", json!({ "path": "scan.pdf" }));

    assert_eq!(response["result"]["isError"], true);
    let error = &response["result"]["structuredContent"]["error"];
    assert_eq!(error["code"], "pdf_image_required");
    assert_eq!(error["retryable"], false);
    let retry = &error["details"]["retry_with"][0];
    assert_eq!(retry["pdf_mode"], "image");
    assert_eq!(retry["pages"], "1");
    assert!(
        retry["pdf_source_id"]
            .as_str()
            .is_some_and(|id| id.len() == 16)
    );
    session.close();
}

/// Every successful PDF response reports the token a continuation must replay, not only
/// the offset-based single-page resume.
#[test]
fn pdf_text_read_reports_its_source_id_over_real_stdio() {
    let fixture = tempfile::tempdir().expect("fixture");
    fs::write(fixture.path().join("document.pdf"), pdf_with_text()).expect("PDF fixture");
    let mut session = Session::start_at(fixture.path());
    session.send(&modern_request(1, "server/discover", empty_params()));
    assert_eq!(session.receive()["id"], 1);

    let response = call_tool(&mut session, 2, "read", json!({ "path": "document.pdf" }));
    assert_eq!(response["result"]["isError"], false);
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .expect("text block");
    let source = text
        .lines()
        .find_map(|line| line.strip_prefix("Source: "))
        .expect("source id line");
    assert!(text.contains("Mode: auto"));

    let stale = call_tool(
        &mut session,
        3,
        "read",
        json!({
            "path": "document.pdf",
            "pages": "1",
            "pdf_source_id": "0000000000000000"
        }),
    );
    assert_eq!(stale["result"]["isError"], true);

    let matching = call_tool(
        &mut session,
        4,
        "read",
        json!({
            "path": "document.pdf",
            "pages": "1",
            "pdf_source_id": source
        }),
    );
    assert_eq!(matching["result"]["isError"], false);
    session.close();
}

/// Pages large enough that rendering them is slow, so a concurrent PDF call reliably
/// finds the gate held rather than racing a fixture that finishes first.
fn slow_render_pdf(pages: usize) -> Vec<u8> {
    let mut bodies: Vec<Vec<u8>> = Vec::new();
    let page_ids: Vec<usize> = (0..pages).map(|index| 4 + index * 2).collect();
    let kids = page_ids
        .iter()
        .map(|id| format!("{id} 0 R"))
        .collect::<Vec<_>>()
        .join(" ");
    bodies.push(b"<< /Type /Catalog /Pages 2 0 R >>".to_vec());
    bodies.push(format!("<< /Type /Pages /Kids [{kids}] /Count {pages} >>").into_bytes());
    bodies.push(
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>"
            .to_vec(),
    );
    for (index, page_id) in page_ids.iter().enumerate() {
        let content_id = page_id + 1;
        bodies.push(
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 5000 5000] /Resources \
                 << /Font << /F1 3 0 R >> >> /Contents {content_id} 0 R >>"
            )
            .into_bytes(),
        );
        let mut operations = String::new();
        for step in 0..400 {
            let offset = step * 12;
            let shade = step % 100;
            let _ = write!(
                operations,
                "0.{shade:02} 0.3 0.7 rg\n{offset} {offset} 900 900 re\nf\n"
            );
        }
        let _ = write!(
            operations,
            "BT /F1 96 Tf 1 0 0 1 200 4200 Tm (Slow page {}) Tj ET",
            index + 1
        );
        let mut stream = format!("<< /Length {} >>\nstream\n", operations.len()).into_bytes();
        stream.extend_from_slice(operations.as_bytes());
        stream.extend_from_slice(b"\nendstream");
        bodies.push(stream);
    }

    let mut pdf = b"%PDF-1.7\n".to_vec();
    let mut offsets = vec![0_usize; bodies.len() + 1];
    for (index, body) in bodies.iter().enumerate() {
        let id = index + 1;
        offsets[id] = pdf.len();
        pdf.extend_from_slice(format!("{id} 0 obj\n").as_bytes());
        pdf.extend_from_slice(body);
        pdf.extend_from_slice(b"\nendobj\n");
    }
    let size = bodies.len() + 1;
    let xref = pdf.len();
    pdf.extend_from_slice(format!("xref\n0 {size}\n0000000000 65535 f \n").as_bytes());
    for offset in offsets.iter().skip(1) {
        pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!("trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n").as_bytes(),
    );
    pdf.extend_from_slice(format!("{xref}\n%%EOF\n").as_bytes());
    pdf
}

/// PDF work is single-slot. A second concurrent PDF must fail with a delay hint rather
/// than queue, and a plain text read must not be affected by either.
#[test]
fn concurrent_pdf_calls_are_gated_while_text_reads_stay_unblocked() {
    let fixture = tempfile::tempdir().expect("fixture");
    fs::write(fixture.path().join("slow.pdf"), slow_render_pdf(4)).expect("PDF fixture");
    fs::write(fixture.path().join("notes.txt"), "alpha\nbeta\n").expect("text fixture");
    let mut session = Session::start_at(fixture.path());
    session.send(&modern_request(1, "server/discover", empty_params()));
    assert_eq!(session.receive()["id"], 1);

    let render = json!({ "path": "slow.pdf", "pdf_mode": "image", "pages": "1-4" });
    for id in 2..=4_u64 {
        let mut call = empty_params();
        call.insert("name".to_owned(), json!("read"));
        call.insert("arguments".to_owned(), render.clone());
        session.send(&modern_request(id, "tools/call", call));
    }
    let mut text_call = empty_params();
    text_call.insert("name".to_owned(), json!("read"));
    text_call.insert("arguments".to_owned(), json!({ "path": "notes.txt" }));
    session.send(&modern_request(5, "tools/call", text_call));

    let mut responses = std::collections::BTreeMap::new();
    for _ in 0..4 {
        let response = session.receive();
        let id = response["id"].as_u64().expect("response id");
        responses.insert(id, response);
    }

    let text = &responses[&5];
    assert_eq!(
        text["result"]["isError"], false,
        "a text read must not be blocked by PDF work: {text}"
    );

    let mut succeeded = 0;
    let mut rejected = 0;
    for id in 2..=4_u64 {
        let response = &responses[&id];
        if response["result"]["isError"] == json!(false) {
            succeeded += 1;
            continue;
        }
        rejected += 1;
        let error = &response["result"]["structuredContent"]["error"];
        assert_eq!(
            error["code"], "resource_busy",
            "a gated PDF call must report resource_busy, got {error}"
        );
        assert_eq!(error["retryable"], true);
        assert_eq!(error["details"]["permit"], "pdf_concurrency");
        assert!(
            error["details"]["retry_after_ms"]
                .as_u64()
                .is_some_and(|ms| ms > 0),
            "resource_busy must carry a retry delay: {error}"
        );
    }
    assert!(succeeded >= 1, "at least one PDF call must be admitted");
    assert!(
        rejected >= 1,
        "three concurrent renders of a slow fixture must saturate the single PDF slot"
    );
    session.close();
}

#[test]
fn oversized_valid_frame_closes_stdio_transport() {
    let mut session = Session::start();
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
            session.child.kill().expect("kill hung server");
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
            .read_line(&mut line)
            .expect("read closed stdout"),
        0
    );
}

#[test]
fn eof_process_child_fixture() {
    if std::env::var("CODEXSHIM_EOF_FIXTURE").as_deref() != Ok("child") {
        return;
    }
    let pid_file = std::env::var_os("CODEXSHIM_EOF_PID_FILE").expect("fixture PID file");
    std::fs::write(pid_file, std::process::id().to_string()).expect("write fixture PID");
    thread::sleep(Duration::from_secs(30));
}

#[test]
fn stdin_eof_cancels_in_flight_process_and_exits_server() {
    let fixture = tempfile::tempdir().expect("fixture");
    let pid_file = fixture.path().join("eof-child.pid");
    let executable = std::env::current_exe().expect("integration test executable");
    let mut session = Session::start();
    session.send(&modern_request(1, "server/discover", empty_params()));
    assert_eq!(session.receive()["id"], 1);

    let mut call = empty_params();
    call.insert("name".to_owned(), json!("run_program"));
    call.insert(
        "arguments".to_owned(),
        json!({
            "program": executable,
            "args": ["--exact", "eof_process_child_fixture", "--nocapture"],
            "cwd": env!("CARGO_MANIFEST_DIR"),
            "env": {
                "CODEXSHIM_EOF_FIXTURE": "child",
                "CODEXSHIM_EOF_PID_FILE": pid_file,
            },
            "timeout_ms": 30_000,
        }),
    );
    session.send(&modern_request(2, "tools/call", call));

    let child_start_deadline = Instant::now() + Duration::from_secs(5);
    while !pid_file.exists() && Instant::now() < child_start_deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(pid_file.exists(), "in-flight child did not start");
    let child_pid = std::fs::read_to_string(&pid_file)
        .expect("child PID")
        .trim()
        .parse::<u32>()
        .expect("numeric child PID");

    session.stdin.take();
    let shutdown_deadline = Instant::now() + Duration::from_secs(12);
    let status = loop {
        if let Some(status) = session.child.try_wait().expect("poll server") {
            break status;
        }
        if Instant::now() >= shutdown_deadline {
            session.child.kill().expect("kill hung server");
            panic!("server did not exit within shutdown and cleanup bounds");
        }
        thread::sleep(Duration::from_millis(10));
    };
    assert!(status.success(), "server exited with {status}");

    let child_exit_deadline = Instant::now() + Duration::from_secs(2);
    while process_is_running(child_pid) && Instant::now() < child_exit_deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !process_is_running(child_pid),
        "in-flight child survived server EOF shutdown"
    );
}

#[test]
fn process_overload_is_fail_fast_and_preserves_resource_busy_contract() {
    let fixture = tempfile::tempdir().expect("fixture");
    let executable = std::env::current_exe().expect("integration test executable");
    let mut session = Session::start_with_process_calls(2);
    session.send(&modern_request(1, "server/discover", empty_params()));
    assert_eq!(session.receive()["id"], 1);

    let mut pid_files = Vec::new();
    for id in [2_u64, 3_u64] {
        let pid_file = fixture.path().join(format!("overload-child-{id}.pid"));
        let mut call = empty_params();
        call.insert("name".to_owned(), json!("run_program"));
        call.insert(
            "arguments".to_owned(),
            json!({
                "program": executable,
                "args": ["--exact", "eof_process_child_fixture", "--nocapture"],
                "cwd": env!("CARGO_MANIFEST_DIR"),
                "env": {
                    "CODEXSHIM_EOF_FIXTURE": "child",
                    "CODEXSHIM_EOF_PID_FILE": pid_file,
                },
                "timeout_ms": 30_000,
            }),
        );
        session.send(&modern_request(id, "tools/call", call));
        pid_files.push(pid_file);
    }
    let active_deadline = Instant::now() + Duration::from_secs(5);
    while pid_files.iter().any(|path| !path.exists()) && Instant::now() < active_deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        pid_files.iter().all(|path| path.exists()),
        "two process calls did not occupy the documented class capacity"
    );

    let mut overflow = empty_params();
    overflow.insert("name".to_owned(), json!("run_program"));
    overflow.insert(
        "arguments".to_owned(),
        json!({
            "program": executable,
            "args": ["--exact", "eof_process_child_fixture", "--nocapture"],
            "cwd": env!("CARGO_MANIFEST_DIR"),
            "timeout_ms": 30_000,
        }),
    );
    let started = Instant::now();
    session.send(&modern_request(4, "tools/call", overflow));
    let response = session.receive();
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "overload response waited for process capacity"
    );
    assert_eq!(response["id"], 4);
    assert_eq!(response["result"]["isError"], true);
    assert_eq!(
        response["result"]["structuredContent"]["error"]["code"],
        "resource_busy"
    );
    assert_eq!(
        response["result"]["structuredContent"]["error"]["retryable"],
        true
    );
    session.close();

    for pid_file in pid_files {
        let pid = std::fs::read_to_string(pid_file)
            .expect("child PID")
            .trim()
            .parse::<u32>()
            .expect("numeric child PID");
        assert!(
            !process_is_running(pid),
            "cancelled overload fixture survived"
        );
    }
}

#[test]
fn default_process_and_read_only_capacity_can_progress_together() {
    const CAPACITY: u64 = 16;

    let fixture = tempfile::tempdir().expect("fixture");
    let executable = std::env::current_exe().expect("integration test executable");
    let mut session = Session::start();
    session.send(&modern_request(1, "server/discover", empty_params()));
    assert_eq!(session.receive()["id"], 1);

    let mut pid_files = Vec::new();
    for id in 2..2 + CAPACITY {
        let pid_file = fixture.path().join(format!("parallel-child-{id}.pid"));
        let mut call = empty_params();
        call.insert("name".to_owned(), json!("run_program"));
        call.insert(
            "arguments".to_owned(),
            json!({
                "program": executable,
                "args": ["--exact", "eof_process_child_fixture", "--nocapture"],
                "cwd": env!("CARGO_MANIFEST_DIR"),
                "env": {
                    "CODEXSHIM_EOF_FIXTURE": "child",
                    "CODEXSHIM_EOF_PID_FILE": pid_file,
                },
                "timeout_ms": 30_000,
            }),
        );
        session.send(&modern_request(id, "tools/call", call));
        pid_files.push(pid_file);
    }
    let active_deadline = Instant::now() + Duration::from_secs(10);
    while pid_files.iter().any(|path| !path.exists()) && Instant::now() < active_deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        pid_files.iter().all(|path| path.exists()),
        "sixteen process calls did not start concurrently"
    );

    let read_ids = (100..100 + CAPACITY).collect::<BTreeSet<_>>();
    for id in &read_ids {
        let mut call = empty_params();
        call.insert("name".to_owned(), json!("read"));
        call.insert(
            "arguments".to_owned(),
            json!({ "path": "Cargo.toml", "line_count": 1 }),
        );
        session.send(&modern_request(*id, "tools/call", call));
    }
    let mut completed_read_ids = BTreeSet::new();
    for _ in 0..CAPACITY {
        let response = session.receive();
        let id = response["id"].as_u64().expect("response id");
        assert!(read_ids.contains(&id), "unexpected response id {id}");
        assert_eq!(response["result"]["isError"], false);
        completed_read_ids.insert(id);
    }
    assert_eq!(completed_read_ids, read_ids);

    let mut overflow = empty_params();
    overflow.insert("name".to_owned(), json!("run_program"));
    overflow.insert(
        "arguments".to_owned(),
        json!({
            "program": executable,
            "args": ["--version"],
            "cwd": env!("CARGO_MANIFEST_DIR"),
        }),
    );
    session.send(&modern_request(200, "tools/call", overflow));
    let response = session.receive();
    assert_eq!(response["id"], 200);
    assert_eq!(
        response["result"]["structuredContent"]["error"]["code"],
        "resource_busy"
    );
    assert_eq!(
        response["result"]["structuredContent"]["error"]["retryable"],
        true
    );

    session.close();
    for pid_file in pid_files {
        let pid = std::fs::read_to_string(pid_file)
            .expect("child PID")
            .trim()
            .parse::<u32>()
            .expect("numeric child PID");
        assert!(
            !process_is_running(pid),
            "parallel fixture survived server shutdown"
        );
    }
}

#[cfg(unix)]
#[test]
#[allow(clippy::zombie_processes)] // The fixture must exit without waiting so the helper escapes its session.
fn unix_outcome_uncertain_parent_fixture() {
    if std::env::var("CODEXSHIM_OUTCOME_UNCERTAIN_FIXTURE").as_deref() != Ok("parent") {
        return;
    }
    let pid_file =
        std::env::var_os("CODEXSHIM_OUTCOME_UNCERTAIN_PID_FILE").expect("fixture PID file");
    let mut command =
        std::process::Command::new(std::env::current_exe().expect("integration test executable"));
    command
        .args([
            "--exact",
            "unix_outcome_uncertain_helper_fixture",
            "--nocapture",
        ])
        .env("CODEXSHIM_OUTCOME_UNCERTAIN_FIXTURE", "helper")
        .env("CODEXSHIM_OUTCOME_UNCERTAIN_PID_FILE", &pid_file)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn().expect("spawn session-escaped helper");
    let deadline = Instant::now() + Duration::from_secs(2);
    while !std::path::Path::new(&pid_file).exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        std::path::Path::new(&pid_file).exists(),
        "session-escaped helper did not record its PID"
    );
}

#[cfg(unix)]
#[test]
fn unix_outcome_uncertain_helper_fixture() {
    if std::env::var("CODEXSHIM_OUTCOME_UNCERTAIN_FIXTURE").as_deref() != Ok("helper") {
        return;
    }
    let pid_file =
        std::env::var_os("CODEXSHIM_OUTCOME_UNCERTAIN_PID_FILE").expect("fixture PID file");
    std::fs::write(pid_file, std::process::id().to_string()).expect("write helper PID");
    thread::sleep(Duration::from_secs(30));
}

#[cfg(unix)]
#[test]
fn session_escaped_descendant_preserves_outcome_uncertain_wire_contract() {
    let fixture = tempfile::tempdir().expect("fixture");
    let pid_file = fixture.path().join("session-escaped.pid");
    let executable = std::env::current_exe().expect("integration test executable");
    let mut session = Session::start();
    session.send(&modern_request(1, "server/discover", empty_params()));
    assert_eq!(session.receive()["id"], 1);

    let mut call = empty_params();
    call.insert("name".to_owned(), json!("run_program"));
    call.insert(
        "arguments".to_owned(),
        json!({
            "program": executable,
            "args": ["--exact", "unix_outcome_uncertain_parent_fixture", "--nocapture"],
            "cwd": env!("CARGO_MANIFEST_DIR"),
            "env": {
                "CODEXSHIM_OUTCOME_UNCERTAIN_FIXTURE": "parent",
                "CODEXSHIM_OUTCOME_UNCERTAIN_PID_FILE": pid_file,
            },
            "timeout_ms": 10_000,
        }),
    );
    let started = Instant::now();
    session.send(&modern_request(2, "tools/call", call));
    let helper_start_deadline = Instant::now() + Duration::from_secs(3);
    while !pid_file.exists() && Instant::now() < helper_start_deadline {
        thread::sleep(Duration::from_millis(10));
    }
    let helper_pid = std::fs::read_to_string(&pid_file)
        .expect("helper PID")
        .trim()
        .parse::<u32>()
        .expect("numeric helper PID");
    let mut helper = EscapedHelper::new(helper_pid);
    let response = session.receive();
    assert!(
        started.elapsed() < Duration::from_secs(7),
        "uncertain cleanup exceeded the shared deadline"
    );
    helper.terminate();

    assert_eq!(response["id"], 2);
    assert_eq!(response["result"]["isError"], true);
    assert_eq!(
        response["result"]["structuredContent"]["error"]["code"],
        "outcome_uncertain"
    );
    assert_eq!(
        response["result"]["structuredContent"]["error"]["retryable"],
        false
    );
    assert_eq!(
        response["result"]["structuredContent"]["error"]["details"]["termination_outcome"],
        "uncertain"
    );
    assert_eq!(
        response["result"]["structuredContent"]["error"]["details"]["containment_scope"],
        "process_group"
    );
    assert!(
        !process_is_running(helper_pid),
        "fixture failed to clean up its escaped helper"
    );
    session.close();
}

#[cfg(unix)]
struct EscapedHelper {
    pid: Option<u32>,
}

#[cfg(unix)]
impl EscapedHelper {
    fn new(pid: u32) -> Self {
        Self { pid: Some(pid) }
    }

    fn terminate(&mut self) {
        let Some(pid) = self.pid.take() else {
            return;
        };
        let pid_i32 = i32::try_from(pid).expect("helper PID fits pid_t");
        // SAFETY: The PID belongs to the fixture-created escaped helper.
        unsafe { libc::kill(pid_i32, libc::SIGKILL) };
        let deadline = Instant::now() + Duration::from_secs(2);
        while process_is_running(pid) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
    }
}

#[cfg(unix)]
impl Drop for EscapedHelper {
    fn drop(&mut self) {
        self.terminate();
    }
}

#[cfg(unix)]
fn process_is_running(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: Signal zero performs a read-only existence check for the numeric PID.
    unsafe { libc::kill(pid, 0) == 0 }
}

#[cfg(windows)]
fn process_is_running(pid: u32) -> bool {
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    };

    const STILL_ACTIVE_EXIT_CODE: u32 = 259;
    // SAFETY: OpenProcess receives a numeric PID and the returned handle is checked and closed.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return false;
    }
    let mut exit_code = 0_u32;
    // SAFETY: process is valid and exit_code points to writable memory.
    let succeeded = unsafe { GetExitCodeProcess(process, &raw mut exit_code) } != 0;
    // SAFETY: process is an owned handle returned by OpenProcess.
    unsafe { CloseHandle(process) };
    succeeded && exit_code == STILL_ACTIVE_EXIT_CODE
}

#[test]
fn normal_tools_reject_unmanaged_paths_outside_the_startup_root() {
    let mut session = Session::start();
    session.send(&modern_request(1, "server/discover", empty_params()));
    assert_eq!(session.receive()["id"], 1);

    let outside = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("repository parent")
        .to_string_lossy()
        .into_owned();
    let requests = [
        ("read", json!({ "path": outside })),
        ("grep", json!({ "path": outside, "pattern": "codexshim" })),
        ("glob", json!({ "path": outside, "pattern": "**/*" })),
    ];

    for (offset, (name, arguments)) in requests.into_iter().enumerate() {
        let id = u64::try_from(offset).expect("request offset") + 2;
        let response = call_tool(&mut session, id, name, arguments);
        assert_eq!(response["id"], id);
        assert_eq!(response["result"]["isError"], true);
        assert!(
            response["result"]["content"][0]["text"]
                .as_str()
                .expect("error text")
                .contains("outside the repository root"),
            "unexpected {name} response: {response}"
        );
    }

    session.close();
}

#[test]
fn unrestricted_scope_reads_searches_and_globs_outside_the_startup_root() {
    let fixture = tempfile::tempdir().expect("outside fixture");
    std::fs::write(fixture.path().join("alpha.rs"), "pub fn needle() {}\n").expect("source");
    std::fs::write(fixture.path().join("beta.txt"), "needle\n").expect("text");
    let base = fixture.path().to_string_lossy().into_owned();
    let source = fixture
        .path()
        .join("alpha.rs")
        .to_string_lossy()
        .into_owned();

    let mut session = Session::start_unrestricted();
    session.send(&modern_request(1, "server/discover", empty_params()));
    let discover = session.receive();
    assert!(
        discover["result"]["instructions"]
            .as_str()
            .expect("instructions")
            .contains("Local filesystem")
    );

    let read = call_tool(&mut session, 2, "read", json!({ "path": source }));
    assert_eq!(read["result"]["isError"], false);
    assert!(
        read["result"]["content"][0]["text"]
            .as_str()
            .expect("read text")
            .contains("pub fn needle")
    );

    let grep = call_tool(
        &mut session,
        3,
        "grep",
        json!({ "path": base, "pattern": "needle", "glob": "*.rs" }),
    );
    assert_eq!(grep["result"]["isError"], false);
    let grep_text = grep["result"]["content"][0]["text"]
        .as_str()
        .expect("grep text");
    assert!(grep_text.contains("alpha.rs"));
    assert!(!grep_text.contains("beta.txt"));

    let glob = call_tool(
        &mut session,
        4,
        "glob",
        json!({ "path": base, "pattern": "*.rs" }),
    );
    assert_eq!(glob["result"]["isError"], false);
    let glob_text = glob["result"]["content"][0]["text"]
        .as_str()
        .expect("glob text");
    assert!(glob_text.contains("alpha.rs"));
    assert!(!glob_text.contains("beta.txt"));

    let process = call_tool(
        &mut session,
        5,
        "run_program",
        json!({ "program": "cargo", "args": ["--version"], "cwd": base }),
    );
    assert_eq!(process["result"]["isError"], false);
    assert!(
        process["result"]["content"][0]["text"]
            .as_str()
            .expect("process output")
            .contains("cargo ")
    );

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
    let mut session = Session::start_strict();
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

    session.stdin.take();
    let status = session.child.wait().expect("wait for rejected server");
    assert!(!status.success());
}

#[test]
fn default_compatibility_uses_native_legacy_initialize_lifecycle() {
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
        ["read", "grep", "glob", "run_program", "bash"]
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
            .contains("1\tinclude!(\"cli/transport.rs\");")
    );
    assert!(
        read["result"].get("structuredContent").is_none(),
        "legacy read success must not emit structured content"
    );
    session.close();
}
