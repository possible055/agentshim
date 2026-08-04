use std::{
    fs::{self, File},
    io::{BufRead, BufReader, BufWriter, Write},
    path::Path,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde_json::{Map, Value, json};

struct Session {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    transcript: BufWriter<File>,
}

impl Session {
    fn start(root: &Path, transcript: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_codexshim"))
            .arg("serve")
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start soak server");
        Self {
            stdin: child.stdin.take().expect("server stdin"),
            stdout: BufReader::new(child.stdout.take().expect("server stdout")),
            child,
            transcript: BufWriter::new(File::create(transcript).expect("create transcript")),
        }
    }

    fn send(&mut self, message: &Value) {
        serde_json::to_writer(&mut self.stdin, message).expect("write request");
        self.stdin.write_all(b"\n").expect("write request newline");
        self.stdin.flush().expect("flush request");
    }

    fn receive(&mut self) -> Value {
        let mut line = String::new();
        let count = self.stdout.read_line(&mut line).expect("read response");
        assert_ne!(count, 0, "server closed before response");
        self.transcript
            .write_all(line.as_bytes())
            .expect("write transcript");
        serde_json::from_str(&line).expect("protocol output must be JSON")
    }

    fn call(&mut self, id: u64, name: &str, arguments: Value) -> Value {
        let mut params = Map::new();
        params.insert("name".to_owned(), json!(name));
        params.insert("arguments".to_owned(), arguments);
        self.send(&modern_request(id, "tools/call", params));
        let response = self.receive();
        assert_eq!(response["id"], id);
        response
    }

    fn resources(&self) -> ResourceSample {
        resource_sample(&self.child)
    }

    fn close(mut self, transcript: &Path) {
        drop(self.stdin);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().expect("poll soak server") {
                assert!(status.success(), "soak server exited with {status}");
                break;
            }
            assert!(Instant::now() < deadline, "soak server did not stop at EOF");
            thread::sleep(Duration::from_millis(10));
        }
        self.transcript.flush().expect("flush transcript");
        for (index, line) in BufReader::new(File::open(transcript).expect("open transcript"))
            .lines()
            .enumerate()
        {
            serde_json::from_str::<Value>(&line.expect("transcript line"))
                .unwrap_or_else(|error| panic!("invalid transcript line {}: {error}", index + 1));
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct ResourceSample {
    memory_bytes: usize,
    handles_or_fds: usize,
    threads: usize,
}

#[derive(Clone, Copy)]
enum Workload {
    Full,
    ReadOnly,
}

fn modern_request(id: u64, method: &str, mut params: Map<String, Value>) -> Value {
    params.insert(
        "_meta".to_owned(),
        json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientInfo": {
                "name": "codexshim-soak",
                "version": "1.0.0"
            },
            "io.modelcontextprotocol/clientCapabilities": {}
        }),
    );
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

fn run_soak(duration: Duration, interval: Duration, transcript: &Path, workload: Workload) {
    let fixture = tempfile::Builder::new()
        .prefix("codexshim-soak-")
        .tempdir_in(env!("CARGO_MANIFEST_DIR"))
        .expect("soak fixture");
    fs::create_dir(fixture.path().join("src")).expect("source directory");
    fs::write(
        fixture.path().join("src/lib.rs"),
        "pub fn needle() -> usize { 42 }\n",
    )
    .expect("source fixture");
    fs::write(
        fixture.path().join("large.txt"),
        "large output line\n".repeat(4_000),
    )
    .expect("large fixture");

    let mut session = Session::start(fixture.path(), transcript);
    session.send(&modern_request(1, "server/discover", Map::new()));
    assert_eq!(session.receive()["id"], 1);
    let started = Instant::now();
    let mut next_id = 10_u64;
    let mut samples = Vec::new();
    let mut round = 0_u64;
    while started.elapsed() < duration || round == 0 {
        exercise_round(&mut session, &mut next_id, fixture.path(), round, workload);
        samples.push(session.resources());
        round += 1;
        if started.elapsed() < duration {
            thread::sleep(interval);
        }
    }
    assert_resource_quiescence(&samples);
    session.close(transcript);
    println!(
        "{}",
        json!({
            "duration_ms": started.elapsed().as_millis(),
            "rounds": round,
            "workload": match workload { Workload::Full => "full", Workload::ReadOnly => "read-only" },
            "samples": samples.iter().map(|sample| json!({
                "handles_or_fds": sample.handles_or_fds,
                "memory_bytes": sample.memory_bytes,
                "threads": sample.threads,
            })).collect::<Vec<_>>(),
            "transcript": transcript,
        })
    );
}

fn exercise_round(
    session: &mut Session,
    next_id: &mut u64,
    root: &Path,
    round: u64,
    workload: Workload,
) {
    assert_success(&session.call(take_id(next_id), "read", json!({ "path": "src/lib.rs" })));
    assert_success(&session.call(
        take_id(next_id),
        "grep",
        json!({ "pattern": "needle", "path": "src", "fixed_strings": true }),
    ));
    assert_success(&session.call(take_id(next_id), "glob", json!({ "pattern": "**/*.rs" })));
    if matches!(workload, Workload::Full) {
        assert_success(&session.call(
            take_id(next_id),
            "run_process",
            json!({
                "program": env!("CARGO_BIN_EXE_codexshim"),
                "args": ["--version"],
                "timeout_ms": 5000
            }),
        ));
    }
    assert_success(&session.call(
        take_id(next_id),
        "read",
        json!({ "path": "large.txt", "line_count": 2000 }),
    ));
    let invalid = session.call(
        take_id(next_id),
        "read",
        json!({ "path": "src/lib.rs", "unexpected": true }),
    );
    assert_eq!(invalid["result"]["isError"], true);

    churn(root, round);
    if round % 5 == 0 {
        parallel_burst(session, next_id);
        assert_cancellation(session, next_id);
        if matches!(workload, Workload::Full) {
            assert_timeout(session, next_id);
        }
    }
}

fn take_id(next_id: &mut u64) -> u64 {
    let id = *next_id;
    *next_id += 1;
    id
}

fn assert_success(response: &Value) {
    assert_eq!(response["result"]["isError"], false);
    assert_eq!(response["result"]["resultType"], "complete");
}

fn churn(root: &Path, round: u64) {
    let source = root.join("churn.tmp");
    let renamed = root.join("churn-renamed.tmp");
    fs::write(&source, format!("round {round}\n")).expect("write churn fixture");
    fs::rename(&source, &renamed).expect("rename churn fixture");
    fs::remove_file(renamed).expect("remove churn fixture");
}

fn parallel_burst(session: &mut Session, next_id: &mut u64) {
    let mut ids = Vec::new();
    for index in 0..8 {
        let id = take_id(next_id);
        ids.push(id);
        let mut params = Map::new();
        if index % 2 == 0 {
            params.insert("name".to_owned(), json!("glob"));
            params.insert("arguments".to_owned(), json!({ "pattern": "**/*" }));
        } else {
            params.insert("name".to_owned(), json!("grep"));
            params.insert(
                "arguments".to_owned(),
                json!({ "pattern": "needle", "fixed_strings": true }),
            );
        }
        session.send(&modern_request(id, "tools/call", params));
    }
    let mut received = Vec::new();
    for _ in 0..8 {
        let response = session.receive();
        assert_eq!(response["result"]["isError"], false);
        received.push(response["id"].as_u64().expect("response id"));
    }
    ids.sort_unstable();
    received.sort_unstable();
    assert_eq!(received, ids);
}

fn assert_cancellation(session: &mut Session, next_id: &mut u64) {
    let cancelled_id = take_id(next_id);
    let mut call = Map::new();
    call.insert("name".to_owned(), json!("grep"));
    call.insert(
        "arguments".to_owned(),
        json!({ "pattern": "never-return", "path": "." }),
    );
    session.send(&modern_request(cancelled_id, "tools/call", call));
    session.send(&json!({
        "jsonrpc": "2.0",
        "method": "notifications/cancelled",
        "params": { "requestId": cancelled_id, "reason": "soak cancellation" }
    }));
    let list_id = take_id(next_id);
    session.send(&modern_request(list_id, "tools/list", Map::new()));
    assert_eq!(session.receive()["id"], list_id);
}

fn assert_timeout(session: &mut Session, next_id: &mut u64) {
    let (program, args) = sleeping_program();
    let response = session.call(
        take_id(next_id),
        "run_process",
        json!({ "program": program, "args": args, "timeout_ms": 20 }),
    );
    assert_eq!(response["result"]["isError"], true);
}

#[cfg(unix)]
fn sleeping_program() -> (&'static str, Vec<&'static str>) {
    ("/bin/sh", vec!["-c", "sleep 30"])
}

#[cfg(windows)]
fn sleeping_program() -> (&'static str, Vec<&'static str>) {
    ("ping.exe", vec!["127.0.0.1", "-n", "30"])
}

fn assert_resource_quiescence(samples: &[ResourceSample]) {
    let first = samples.first().expect("resource sample");
    let last = samples.last().expect("resource sample");
    assert!(
        last.memory_bytes <= first.memory_bytes.saturating_add(128 * 1024 * 1024),
        "memory grew beyond the 128 MiB release bound"
    );
    assert!(
        last.handles_or_fds <= first.handles_or_fds.saturating_add(16),
        "handle/file descriptor count did not return near baseline"
    );
}

#[cfg(unix)]
fn resource_sample(child: &Child) -> ResourceSample {
    let status = fs::read_to_string(format!("/proc/{}/status", child.id())).expect("proc status");
    let value = |name: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|value| value.split_whitespace().next())
            .and_then(|value| value.parse::<usize>().ok())
            .expect("proc metric")
    };
    let handles_or_fds = fs::read_dir(format!("/proc/{}/fd", child.id()))
        .expect("proc fd")
        .count();
    ResourceSample {
        memory_bytes: value("VmRSS:") * 1024,
        handles_or_fds,
        threads: value("Threads:"),
    }
}

#[cfg(windows)]
fn resource_sample(child: &Child) -> ResourceSample {
    use std::{mem::size_of, os::windows::io::AsRawHandle};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First,
                Thread32Next,
            },
            ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX},
            Threading::GetProcessHandleCount,
        },
    };

    struct Snapshot(windows_sys::Win32::Foundation::HANDLE);

    impl Drop for Snapshot {
        fn drop(&mut self) {
            // SAFETY: The handle is owned by this guard and remains valid until this drop.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    // SAFETY: The flags satisfy the documented system-wide thread snapshot contract.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    assert_ne!(snapshot, INVALID_HANDLE_VALUE, "thread snapshot failed");
    let snapshot = Snapshot(snapshot);
    let mut entry = THREADENTRY32 {
        dwSize: u32::try_from(size_of::<THREADENTRY32>()).expect("thread entry size"),
        ..THREADENTRY32::default()
    };
    // SAFETY: The snapshot is live and entry points to a correctly sized writable structure.
    assert_ne!(unsafe { Thread32First(snapshot.0, &raw mut entry) }, 0);
    let mut threads = 0_usize;
    loop {
        if entry.th32OwnerProcessID == child.id() {
            threads = threads.saturating_add(1);
        }
        // SAFETY: The snapshot and writable entry remain live for enumeration.
        if unsafe { Thread32Next(snapshot.0, &raw mut entry) } == 0 {
            break;
        }
    }

    let handle = child.as_raw_handle().cast();
    let mut handles = 0_u32;
    assert_ne!(
        unsafe { GetProcessHandleCount(handle, &raw mut handles) },
        0
    );
    let mut memory = PROCESS_MEMORY_COUNTERS_EX {
        cb: u32::try_from(size_of::<PROCESS_MEMORY_COUNTERS_EX>()).expect("memory counter size"),
        ..PROCESS_MEMORY_COUNTERS_EX::default()
    };
    assert_ne!(
        unsafe {
            K32GetProcessMemoryInfo(
                handle,
                (&raw mut memory).cast(),
                u32::try_from(size_of::<PROCESS_MEMORY_COUNTERS_EX>())
                    .expect("memory counter size"),
            )
        },
        0
    );
    ResourceSample {
        memory_bytes: memory.PrivateUsage,
        handles_or_fds: handles as usize,
        threads,
    }
}

#[test]
fn short_soak_harness_smoke() {
    let output = tempfile::tempdir().expect("soak smoke output");
    let transcript = output.path().join("transcript.jsonl");
    run_soak(
        Duration::from_secs(1),
        Duration::from_millis(10),
        &transcript,
        Workload::Full,
    );
}

#[test]
#[ignore = "24-hour release gate"]
fn twenty_four_hour_soak() {
    let output = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/soak");
    fs::create_dir_all(&output).expect("soak output directory");
    run_soak(
        Duration::from_secs(24 * 60 * 60),
        Duration::from_secs(10),
        &output.join(format!("{}.jsonl", std::env::consts::OS)),
        Workload::Full,
    );
}

#[test]
#[ignore = "24-hour read-only release gate"]
fn twenty_four_hour_read_only_soak() {
    let output = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/soak");
    fs::create_dir_all(&output).expect("soak output directory");
    run_soak(
        Duration::from_secs(24 * 60 * 60),
        Duration::from_secs(10),
        &output.join(format!("{}-read-only.jsonl", std::env::consts::OS)),
        Workload::ReadOnly,
    );
}
