use std::{
    collections::BTreeSet,
    fs,
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use codexshim::RuntimeLimits;
use serde_json::{Value, json};

const WARM_SAMPLES: usize = 7;
const MCP_GREP_ENV: &str = "CODEXSHIM_BENCH_MCP_GREP";
const MCP_GREP_FILES_ENV: &str = "CODEXSHIM_BENCH_MCP_GREP_FILES";
const MCP_GREP_GLOB_ENV: &str = "CODEXSHIM_BENCH_MCP_GREP_GLOB";
const MCP_GREP_FILE_BYTES_ENV: &str = "CODEXSHIM_BENCH_MCP_GREP_FILE_BYTES";
const MCP_GREP_DENSITY_ENV: &str = "CODEXSHIM_BENCH_MCP_GREP_DENSITY";
const MCP_GREP_MODE_ENV: &str = "CODEXSHIM_BENCH_MCP_GREP_MODE";
const MCP_GREP_STORAGE_ENV: &str = "CODEXSHIM_BENCH_MCP_GREP_STORAGE";
const MCP_GREP_WARM_SAMPLES_ENV: &str = "CODEXSHIM_BENCH_MCP_GREP_WARM_SAMPLES";
const BENCH_COMMIT_ENV: &str = "CODEXSHIM_BENCH_COMMIT";
const BENCH_WORKTREE_ENV: &str = "CODEXSHIM_BENCH_WORKTREE";
const COLD_LIMIT_ENV: &str = "CODEXSHIM_BENCH_MAX_STDIO_COLD_MS";
const P95_LIMIT_ENV: &str = "CODEXSHIM_BENCH_MAX_STDIO_P95_MS";
const PROCESS_LIMIT_ENV: &str = "CODEXSHIM_BENCH_MAX_PROCESS_MS";

fn main() {
    if std::env::var_os(MCP_GREP_ENV).is_some_and(|value| value == "1") {
        benchmark_mcp_grep();
        return;
    }
    for mode in ["off", "errors", "all"] {
        benchmark_mode(mode);
    }
}

fn benchmark_mcp_grep() {
    let files = std::env::var(MCP_GREP_FILES_ENV).map_or(10_000, |value| {
        value
            .parse::<usize>()
            .expect("MCP grep fixture size is an integer")
    });
    assert!(
        matches!(files, 1_000 | 10_000 | 100_000),
        "{MCP_GREP_FILES_ENV} accepts only 1000,10000,100000"
    );
    let fixture = tempfile::tempdir().expect("MCP grep fixture");
    let repository = fixture.path().join("repository");
    let file_bytes = configured_file_bytes();
    let density = configured_density();
    let mode = configured_mode();
    let storage = configured_storage();
    let warm_samples = configured_mcp_warm_samples();
    create_grep_corpus(
        &repository.join("corpus"),
        files,
        file_bytes,
        density,
        storage,
    );
    let logs = tempfile::tempdir().expect("MCP grep logs");
    let limits = RuntimeLimits::from_env().expect("runtime limits");
    let glob = std::env::var(MCP_GREP_GLOB_ENV).unwrap_or_else(|_| "**/*.rs".to_owned());

    let cold_started = Instant::now();
    let mut session = Session::start_in("off", logs.path(), &repository);
    let resources = ResourceMonitor::start(session.pid());
    let expected = session.grep(1, &glob, mode);
    let cold_ms = cold_started.elapsed().as_secs_f64() * 1_000.0;
    let mut warm_ms = Vec::with_capacity(warm_samples);
    for id in 2..=warm_samples as u64 + 1 {
        let started = Instant::now();
        assert_eq!(
            session.grep(id, &glob, mode),
            expected,
            "MCP grep output changed"
        );
        warm_ms.push(started.elapsed().as_secs_f64() * 1_000.0);
    }
    warm_ms.sort_by(f64::total_cmp);

    let concurrent = [1_usize, 8]
        .into_iter()
        .enumerate()
        .map(|(index, calls)| {
            let mut completion_ms =
                session.grep_batch(100 + index as u64 * 32, calls, &glob, mode, &expected);
            completion_ms.sort_by(f64::total_cmp);
            let elapsed_ms = completion_ms.last().copied().unwrap_or_default();
            json!({
                "calls": calls,
                "elapsed_ms": elapsed_ms,
                "completion_p50_ms": percentile(&completion_ms, 50, 100),
                "completion_p95_ms": percentile(&completion_ms, 95, 100),
                "throughput_per_second": f64::from(u32::try_from(calls).expect("bounded calls"))
                    / (elapsed_ms / 1_000.0),
                "completion_ms": completion_ms,
            })
        })
        .collect::<Vec<_>>();
    let resources = resources.finish();
    assert!(
        resources.peak_working_set_bytes < 1024 * 1024 * 1024,
        "MCP grep process working set exceeded 1 GiB"
    );
    session.close();

    println!(
        "{}",
        json!({
            "benchmark": "mcp_grep_full_scan",
            "binary_commit": std::env::var(BENCH_COMMIT_ENV).unwrap_or_else(|_| "unrecorded".to_owned()),
            "binary_worktree": std::env::var(BENCH_WORKTREE_ENV).unwrap_or_else(|_| "unrecorded".to_owned()),
            "memory_policy": "soft_gate_128_mib",
            "process_rss_hard_limit_bytes": 1024_u64 * 1024 * 1024,
            "fixture_files": files,
            "file_bytes": file_bytes,
            "density": density,
            "mode": mode,
            "storage": storage,
            "warm_samples": warm_samples,
            "source_policy": std::env::var("CODEXSHIM_BENCH_GREP_SOURCE").unwrap_or_else(|_| "reader".to_owned()),
            "pathname_reopen": std::env::var("CODEXSHIM_BENCH_GREP_PATHNAME_REOPEN").unwrap_or_else(|_| "off".to_owned()),
            "glob": glob,
            "worker_lanes": limits.worker_lanes,
            "scheduler_threads": limits.scheduler_threads,
            "blocking_threads": limits.blocking_threads,
            "cold_ms": cold_ms,
            "warm_ms": warm_ms,
            "p50_ms": percentile(&warm_ms, 50, 100),
            "p95_ms": percentile(&warm_ms, 95, 100),
            "p99_ms": percentile(&warm_ms, 99, 100),
            "concurrent": concurrent,
            "output_bytes": expected.to_string().len(),
            "output_equivalent": true,
            "resources": {
                "peak_working_set_bytes": resources.peak_working_set_bytes,
                "peak_threads": resources.peak_threads,
                "peak_handles": resources.peak_handles,
                "read_operation_delta": resources.read_operation_delta,
                "read_bytes_delta": resources.read_bytes_delta,
                "write_operation_delta": resources.write_operation_delta,
                "write_bytes_delta": resources.write_bytes_delta,
                "page_fault_delta": resources.page_fault_delta,
                "temp_bytes_high_water": 0,
                "temp_files_high_water": 0,
            },
        })
    );
}

fn create_grep_corpus(
    directory: &Path,
    files: usize,
    file_bytes: usize,
    density: &str,
    storage: &str,
) {
    let templates = directory.parent().expect("corpus parent").join("templates");
    fs::create_dir_all(&templates).expect("MCP grep templates");
    let ordinary = templates.join("ordinary.rs");
    let matching = templates.join("matching.rs");
    fs::write(&ordinary, corpus_content(file_bytes, false)).expect("ordinary template");
    fs::write(&matching, corpus_content(file_bytes, true)).expect("matching template");
    for index in 0..files {
        let shard_index = index / 1_000;
        let shard = directory.join(format!("shard-{shard_index:06}"));
        if index % 1_000 == 0 {
            fs::create_dir_all(&shard).expect("MCP grep shard");
            fs::copy(
                &ordinary,
                templates.join(format!("ordinary-{shard_index:06}.rs")),
            )
            .expect("ordinary shard template");
            fs::copy(
                &matching,
                templates.join(format!("matching-{shard_index:06}.rs")),
            )
            .expect("matching shard template");
        }
        let includes_match = match density {
            "none" => false,
            "rare" => index.is_multiple_of(100),
            "dense" => true,
            _ => unreachable!("density validated"),
        };
        let template = templates.join(format!(
            "{}-{shard_index:06}.rs",
            if includes_match {
                "matching"
            } else {
                "ordinary"
            }
        ));
        let destination = shard.join(format!("file-{index:09}.rs"));
        match storage {
            "hard_link" => fs::hard_link(template, destination).expect("MCP grep hard link"),
            "copy" => {
                fs::copy(template, destination).expect("MCP grep copy");
            }
            _ => unreachable!("storage validated"),
        }
    }
}

fn corpus_content(file_bytes: usize, matching: bool) -> Vec<u8> {
    let line = if matching {
        b"needle-matched\n".as_slice()
    } else {
        b"ordinary-text\n".as_slice()
    };
    let mut content = Vec::with_capacity(file_bytes);
    while content.len().saturating_add(line.len()) <= file_bytes {
        content.extend_from_slice(line);
    }
    content.resize(file_bytes, b'x');
    content
}

fn configured_file_bytes() -> usize {
    let bytes = std::env::var(MCP_GREP_FILE_BYTES_ENV).map_or(1_024, |value| {
        value
            .parse::<usize>()
            .expect("MCP grep file bytes is an integer")
    });
    assert!(
        matches!(bytes, 1_024 | 65_536 | 4_194_304),
        "{MCP_GREP_FILE_BYTES_ENV} accepts only 1024,65536,4194304"
    );
    bytes
}

fn configured_density() -> &'static str {
    match std::env::var(MCP_GREP_DENSITY_ENV).as_deref() {
        Ok("none") | Err(std::env::VarError::NotPresent) => "none",
        Ok("rare") => "rare",
        Ok("dense") => "dense",
        Ok(value) => panic!("{MCP_GREP_DENSITY_ENV} accepts none,rare,dense; got {value}"),
        Err(error) => panic!("{MCP_GREP_DENSITY_ENV} is not valid Unicode: {error}"),
    }
}

fn configured_mode() -> &'static str {
    match std::env::var(MCP_GREP_MODE_ENV).as_deref() {
        Ok("content") | Err(std::env::VarError::NotPresent) => "content",
        Ok("files") => "files",
        Ok("count") => "count",
        Ok(value) => panic!("{MCP_GREP_MODE_ENV} accepts content,files,count; got {value}"),
        Err(error) => panic!("{MCP_GREP_MODE_ENV} is not valid Unicode: {error}"),
    }
}

fn configured_storage() -> &'static str {
    match std::env::var(MCP_GREP_STORAGE_ENV).as_deref() {
        Ok("hard_link") | Err(std::env::VarError::NotPresent) => "hard_link",
        Ok("copy") => "copy",
        Ok(value) => panic!("{MCP_GREP_STORAGE_ENV} accepts hard_link,copy; got {value}"),
        Err(error) => panic!("{MCP_GREP_STORAGE_ENV} is not valid Unicode: {error}"),
    }
}

fn configured_mcp_warm_samples() -> usize {
    let samples = std::env::var(MCP_GREP_WARM_SAMPLES_ENV).map_or(WARM_SAMPLES, |value| {
        value
            .parse::<usize>()
            .expect("MCP grep warm samples is an integer")
    });
    assert!(
        (1..=WARM_SAMPLES).contains(&samples),
        "{MCP_GREP_WARM_SAMPLES_ENV} accepts 1 through {WARM_SAMPLES}"
    );
    samples
}

fn benchmark_mode(mode: &str) {
    let logs = tempfile::tempdir().expect("diagnostic directory");
    let cold_started = Instant::now();
    let mut session = Session::start(mode, logs.path());
    let expected = session.read(1);
    let cold_ms = cold_started.elapsed().as_secs_f64() * 1_000.0;
    let mut warm_ms = Vec::with_capacity(WARM_SAMPLES);
    for id in 2..=WARM_SAMPLES as u64 + 1 {
        let started = Instant::now();
        assert_eq!(session.read(id), expected, "stdio output changed");
        warm_ms.push(started.elapsed().as_secs_f64() * 1_000.0);
    }
    let process_started = Instant::now();
    let process_output = session.run_process(WARM_SAMPLES as u64 + 2);
    let process_ms = process_started.elapsed().as_secs_f64() * 1_000.0;
    let process_concurrent = [1_usize, 8, 16]
        .into_iter()
        .enumerate()
        .map(|(index, calls)| {
            let mut completion_ms = session.run_process_batch(100 + index as u64 * 32, calls);
            completion_ms.sort_by(f64::total_cmp);
            json!({
                "calls": calls,
                "completion_p50_ms": percentile(&completion_ms, 50, 100),
                "completion_p95_ms": percentile(&completion_ms, 95, 100),
                "completion_ms": completion_ms,
            })
        })
        .collect::<Vec<_>>();
    session.close();
    warm_ms.sort_by(f64::total_cmp);
    let p95_ms = percentile(&warm_ms, 95, 100);
    let cold_limit_ms = configured_limit(COLD_LIMIT_ENV, 750.0);
    let p95_limit_ms = configured_limit(P95_LIMIT_ENV, 10.0);
    let process_limit_ms = configured_limit(PROCESS_LIMIT_ENV, 300.0);
    assert!(
        cold_ms <= cold_limit_ms,
        "stdio {mode} cold start {cold_ms:.3} ms exceeds {cold_limit_ms:.3} ms"
    );
    assert!(
        p95_ms <= p95_limit_ms,
        "stdio {mode} p95 {p95_ms:.3} ms exceeds {p95_limit_ms:.3} ms"
    );
    assert!(
        process_ms <= process_limit_ms,
        "stdio {mode} process {process_ms:.3} ms exceeds {process_limit_ms:.3} ms"
    );
    println!(
        "{}",
        json!({
            "benchmark": "stdio_diagnostics",
            "mode": mode,
            "cold_ms": cold_ms,
            "warm_ms": warm_ms,
            "p50_ms": percentile(&warm_ms, 50, 100),
            "p95_ms": p95_ms,
            "p99_ms": percentile(&warm_ms, 99, 100),
            "cold_limit_ms": cold_limit_ms,
            "p95_limit_ms": p95_limit_ms,
            "process_limit_ms": process_limit_ms,
            "output_equivalent": true,
            "process_ms": process_ms,
            "process_concurrent": process_concurrent,
            "process_output_bytes": process_output.to_string().len(),
        })
    );
}

fn configured_limit(name: &str, default: f64) -> f64 {
    let value = std::env::var(name).map_or(default, |value| {
        value
            .parse::<f64>()
            .unwrap_or_else(|_| panic!("{name} must be a number"))
    });
    assert!(value.is_finite() && value > 0.0, "{name} must be positive");
    value
}

struct Session {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl Session {
    fn start(mode: &str, logs: &std::path::Path) -> Self {
        Self::start_in(mode, logs, Path::new(env!("CARGO_MANIFEST_DIR")))
    }

    fn start_in(mode: &str, logs: &Path, root: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_codexshim"))
            .arg("serve")
            .current_dir(root)
            .env("CODEXSHIM_LOG_MODE", mode)
            .env("CODEXSHIM_LOG_DIR", logs)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start stdio server");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        Self {
            child,
            stdin: Some(stdin),
            stdout,
        }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn read(&mut self, id: u64) -> Value {
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {
                "name": "read",
                "arguments": { "path": "Cargo.toml", "line_count": 1000 },
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientInfo": {
                        "name": "stdio-performance",
                        "version": "1.0.0"
                    },
                    "io.modelcontextprotocol/clientCapabilities": {}
                }
            }
        });
        let stdin = self.stdin.as_mut().expect("stdin open");
        serde_json::to_writer(&mut *stdin, &request).expect("request");
        stdin.write_all(b"\n").expect("request newline");
        stdin.flush().expect("request flush");
        let mut response = String::new();
        self.stdout.read_line(&mut response).expect("response");
        let response: Value = serde_json::from_str(&response).expect("JSON-RPC response");
        assert_eq!(response["id"], id);
        response["result"].clone()
    }

    fn grep(&mut self, id: u64, glob: &str, mode: &str) -> Value {
        self.send_grep(id, glob, mode);
        self.receive_tool(id, "grep")
    }

    fn grep_batch(
        &mut self,
        first_id: u64,
        calls: usize,
        glob: &str,
        mode: &str,
        expected: &Value,
    ) -> Vec<f64> {
        let expected_ids = (first_id..first_id + calls as u64).collect::<BTreeSet<_>>();
        let started = Instant::now();
        for id in &expected_ids {
            self.send_grep(*id, glob, mode);
        }
        let mut received_ids = BTreeSet::new();
        let mut completion_ms = Vec::with_capacity(calls);
        for _ in 0..calls {
            let response = self.receive_tool_response("grep");
            let id = response["id"].as_u64().expect("grep response id");
            assert!(expected_ids.contains(&id), "unexpected grep response {id}");
            assert_eq!(response["result"], *expected, "MCP grep output changed");
            received_ids.insert(id);
            completion_ms.push(started.elapsed().as_secs_f64() * 1_000.0);
        }
        assert_eq!(received_ids, expected_ids);
        completion_ms
    }

    fn send_grep(&mut self, id: u64, glob: &str, mode: &str) {
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {
                "name": "grep",
                "arguments": {
                    "pattern": "needle-",
                    "path": "corpus",
                    "glob": glob,
                    "mode": mode,
                    "fixed_strings": true,
                    "limit": 1000
                },
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientInfo": {
                        "name": "stdio-performance",
                        "version": "1.0.0"
                    },
                    "io.modelcontextprotocol/clientCapabilities": {}
                }
            }
        });
        let stdin = self.stdin.as_mut().expect("stdin open");
        serde_json::to_writer(&mut *stdin, &request).expect("grep request");
        stdin.write_all(b"\n").expect("grep request newline");
        stdin.flush().expect("grep request flush");
    }

    fn run_process(&mut self, id: u64) -> Value {
        self.send_process(id);
        self.receive_process(id)
    }

    fn run_process_batch(&mut self, first_id: u64, calls: usize) -> Vec<f64> {
        let expected_ids = (first_id..first_id + calls as u64).collect::<BTreeSet<_>>();
        for id in &expected_ids {
            self.send_process(*id);
        }
        let started = Instant::now();
        let mut received_ids = BTreeSet::new();
        let mut completion_ms = Vec::with_capacity(calls);
        for _ in 0..calls {
            let response = self.receive_process_response();
            let id = response["id"].as_u64().expect("process response id");
            assert!(
                expected_ids.contains(&id),
                "unexpected process response {id}"
            );
            assert_eq!(
                response["result"]["isError"], false,
                "process call failed: {response}"
            );
            received_ids.insert(id);
            completion_ms.push(started.elapsed().as_secs_f64() * 1_000.0);
        }
        assert_eq!(received_ids, expected_ids);
        completion_ms
    }

    fn send_process(&mut self, id: u64) {
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {
                "name": "run_program",
                "arguments": {
                    "program": env!("CARGO_BIN_EXE_codexshim"),
                    "args": ["--version"],
                    "timeout_ms": 30000
                },
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientInfo": {
                        "name": "stdio-performance",
                        "version": "1.0.0"
                    },
                    "io.modelcontextprotocol/clientCapabilities": {}
                }
            }
        });
        let stdin = self.stdin.as_mut().expect("stdin open");
        serde_json::to_writer(&mut *stdin, &request).expect("process request");
        stdin.write_all(b"\n").expect("process request newline");
        stdin.flush().expect("process request flush");
    }

    fn receive_process(&mut self, id: u64) -> Value {
        self.receive_tool(id, "process")
    }

    fn receive_process_response(&mut self) -> Value {
        self.receive_tool_response("process")
    }

    fn receive_tool(&mut self, id: u64, tool: &str) -> Value {
        let response = self.receive_tool_response(tool);
        assert_eq!(response["id"], id);
        assert_eq!(
            response["result"]["isError"], false,
            "{tool} call failed: {response}"
        );
        response["result"].clone()
    }

    fn receive_tool_response(&mut self, tool: &str) -> Value {
        let mut response = String::new();
        self.stdout
            .read_line(&mut response)
            .unwrap_or_else(|error| panic!("{tool} response: {error}"));
        serde_json::from_str(&response)
            .unwrap_or_else(|error| panic!("{tool} JSON-RPC response: {error}"))
    }

    fn close(mut self) {
        self.stdin.take();
        let status = self.child.wait().expect("stdio server exit");
        assert!(status.success(), "stdio server exited with {status}");
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct ProcessSample {
    working_set_bytes: u64,
    peak_working_set_bytes: u64,
    threads: u64,
    handles: u64,
    read_operations: u64,
    read_bytes: u64,
    write_operations: u64,
    write_bytes: u64,
    page_faults: u64,
}

#[derive(Clone, Copy, Debug, Default)]
struct ResourceHighWater {
    peak_working_set_bytes: u64,
    peak_threads: u64,
    peak_handles: u64,
    read_operation_delta: u64,
    read_bytes_delta: u64,
    write_operation_delta: u64,
    write_bytes_delta: u64,
    page_fault_delta: u64,
}

struct ResourceMonitor {
    stop: Arc<AtomicBool>,
    worker: std::thread::JoinHandle<ResourceHighWater>,
}

impl ResourceMonitor {
    fn start(pid: u32) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = std::thread::spawn(move || {
            let first = platform::sample(pid).expect("initial process resource sample");
            let mut last = first;
            let mut high = ResourceHighWater {
                peak_working_set_bytes: first.peak_working_set_bytes.max(first.working_set_bytes),
                peak_threads: first.threads,
                peak_handles: first.handles,
                ..ResourceHighWater::default()
            };
            while !worker_stop.load(Ordering::Acquire) {
                if let Ok(sample) = platform::sample(pid) {
                    high.peak_working_set_bytes = high
                        .peak_working_set_bytes
                        .max(sample.peak_working_set_bytes)
                        .max(sample.working_set_bytes);
                    high.peak_threads = high.peak_threads.max(sample.threads);
                    high.peak_handles = high.peak_handles.max(sample.handles);
                    last = sample;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            high.read_operation_delta = last.read_operations.saturating_sub(first.read_operations);
            high.read_bytes_delta = last.read_bytes.saturating_sub(first.read_bytes);
            high.write_operation_delta =
                last.write_operations.saturating_sub(first.write_operations);
            high.write_bytes_delta = last.write_bytes.saturating_sub(first.write_bytes);
            high.page_fault_delta = last.page_faults.saturating_sub(first.page_faults);
            high
        });
        Self { stop, worker }
    }

    fn finish(self) -> ResourceHighWater {
        self.stop.store(true, Ordering::Release);
        self.worker.join().expect("resource monitor")
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

#[cfg(windows)]
mod platform {
    use std::{io, mem};

    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE},
        System::Threading::{
            GetProcessHandleCount, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
        },
    };

    use super::ProcessSample;

    const TH32CS_SNAPPROCESS: u32 = 0x0000_0002;

    #[repr(C)]
    struct ProcessMemoryCounters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }

    #[repr(C)]
    struct IoCounters {
        read_operations: u64,
        write_operations: u64,
        other_operations: u64,
        read_bytes: u64,
        write_bytes: u64,
        other_bytes: u64,
    }

    #[repr(C)]
    struct ProcessEntry32W {
        size: u32,
        usage: u32,
        process_id: u32,
        default_heap_id: usize,
        module_id: u32,
        threads: u32,
        parent_process_id: u32,
        priority_class_base: i32,
        flags: u32,
        executable_file: [u16; 260],
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn K32GetProcessMemoryInfo(
            process: HANDLE,
            counters: *mut ProcessMemoryCounters,
            size: u32,
        ) -> i32;
        fn GetProcessIoCounters(process: HANDLE, counters: *mut IoCounters) -> i32;
        fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> HANDLE;
        fn Process32FirstW(snapshot: HANDLE, entry: *mut ProcessEntry32W) -> i32;
        fn Process32NextW(snapshot: HANDLE, entry: *mut ProcessEntry32W) -> i32;
    }

    pub fn sample(pid: u32) -> io::Result<ProcessSample> {
        // SAFETY: The PID belongs to the live benchmark child and the handle is checked and closed.
        let process =
            unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ, 0, pid) };
        if process.is_null() {
            return Err(io::Error::last_os_error());
        }
        let result = (|| {
            // SAFETY: Both C structures are initialized before the APIs write into them.
            let mut memory: ProcessMemoryCounters = unsafe { mem::zeroed() };
            memory.cb = u32::try_from(mem::size_of::<ProcessMemoryCounters>())
                .expect("memory counter size");
            // SAFETY: process is valid and memory is writable for its declared size.
            if unsafe { K32GetProcessMemoryInfo(process, &raw mut memory, memory.cb) } == 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: The zeroed structure has the exact layout expected by the API.
            let mut io_counters: IoCounters = unsafe { mem::zeroed() };
            // SAFETY: process is valid and io_counters is writable.
            if unsafe { GetProcessIoCounters(process, &raw mut io_counters) } == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut handles = 0_u32;
            // SAFETY: process is valid and handles is writable.
            if unsafe { GetProcessHandleCount(process, &raw mut handles) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(ProcessSample {
                working_set_bytes: memory.working_set_size as u64,
                peak_working_set_bytes: memory.peak_working_set_size as u64,
                threads: u64::from(process_threads(pid)?),
                handles: u64::from(handles),
                read_operations: io_counters.read_operations,
                read_bytes: io_counters.read_bytes,
                write_operations: io_counters.write_operations,
                write_bytes: io_counters.write_bytes,
                page_faults: u64::from(memory.page_fault_count),
            })
        })();
        // SAFETY: process is the owned handle returned by OpenProcess.
        unsafe { CloseHandle(process) };
        result
    }

    fn process_threads(pid: u32) -> io::Result<u32> {
        // SAFETY: The API has no borrowed pointer arguments and the handle is checked and closed.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: The zeroed C structure receives its required size before enumeration.
        let mut entry: ProcessEntry32W = unsafe { mem::zeroed() };
        entry.size = u32::try_from(mem::size_of::<ProcessEntry32W>()).expect("process entry size");
        // SAFETY: snapshot and entry are valid for enumeration.
        let mut has_entry = unsafe { Process32FirstW(snapshot, &raw mut entry) } != 0;
        let mut threads = None;
        while has_entry {
            if entry.process_id == pid {
                threads = Some(entry.threads);
                break;
            }
            // SAFETY: snapshot and entry remain valid for the next call.
            has_entry = unsafe { Process32NextW(snapshot, &raw mut entry) } != 0;
        }
        // SAFETY: snapshot is an owned handle.
        unsafe { CloseHandle(snapshot) };
        threads.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "benchmark child"))
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use std::{fs, io};

    use super::ProcessSample;

    pub fn sample(pid: u32) -> io::Result<ProcessSample> {
        let status = fs::read_to_string(format!("/proc/{pid}/status"))?;
        let io = fs::read_to_string(format!("/proc/{pid}/io"))?;
        Ok(ProcessSample {
            working_set_bytes: status_value(&status, "VmRSS:")? * 1024,
            peak_working_set_bytes: status_value(&status, "VmHWM:")? * 1024,
            threads: status_value(&status, "Threads:")?,
            handles: fs::read_dir(format!("/proc/{pid}/fd"))?.count() as u64,
            read_operations: io_value(&io, "syscr:")?,
            read_bytes: io_value(&io, "read_bytes:")?,
            write_operations: io_value(&io, "syscw:")?,
            write_bytes: io_value(&io, "write_bytes:")?,
            page_faults: 0,
        })
    }

    fn status_value(status: &str, key: &str) -> io::Result<u64> {
        status
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .and_then(|value| value.split_whitespace().next())
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, key))
    }

    fn io_value(contents: &str, key: &str) -> io::Result<u64> {
        contents
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .and_then(|value| value.trim().parse().ok())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, key))
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod platform {
    use std::io;

    use super::ProcessSample;

    // Keep the same fallible sampling interface as the supported platforms.
    #[allow(clippy::unnecessary_wraps)]
    pub fn sample(_pid: u32) -> io::Result<ProcessSample> {
        Ok(ProcessSample::default())
    }
}

fn percentile(samples: &[f64], numerator: usize, denominator: usize) -> f64 {
    let index = samples
        .len()
        .saturating_mul(numerator)
        .div_ceil(denominator)
        .saturating_sub(1)
        .min(samples.len() - 1);
    samples[index]
}
