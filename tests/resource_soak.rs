use std::{
    collections::BTreeMap,
    env,
    fs::{self, File},
    io::{BufRead, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde_json::{Map, Value, json};

const DEFAULT_ITERATIONS: usize = 100;
const EXTENDED_ITERATIONS: usize = 1_000;

struct Session {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Session {
    fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_codexshim"))
            .arg("serve")
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .env_remove("CODEXSHIM_MCP_COMPATIBILITY")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start codexshim");
        let stdin = child.stdin.take().expect("server stdin");
        let stdout = BufReader::new(child.stdout.take().expect("server stdout"));
        Self {
            child,
            stdin: Some(stdin),
            stdout,
            next_id: 1,
        }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn discover(&mut self) {
        let response = self.request("server/discover", Map::new());
        assert_eq!(response["result"]["capabilities"], json!({ "tools": {} }));
    }

    fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
        let mut params = Map::new();
        params.insert("name".to_owned(), json!(name));
        params.insert("arguments".to_owned(), arguments);
        let response = self.request("tools/call", params);
        assert_eq!(
            response["result"]["isError"], false,
            "{name} failed: {response}"
        );
        assert_eq!(response["result"]["resultType"], "complete");
        response
    }

    fn send_tool(&mut self, name: &str, arguments: Value) -> u64 {
        let mut params = Map::new();
        params.insert("name".to_owned(), json!(name));
        params.insert("arguments".to_owned(), arguments);
        self.send_request("tools/call", params)
    }

    fn request(&mut self, method: &str, params: Map<String, Value>) -> Value {
        let id = self.send_request(method, params);
        self.receive(id)
    }

    fn send_request(&mut self, method: &str, mut params: Map<String, Value>) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        params.insert("_meta".to_owned(), modern_meta());
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let stdin = self.stdin.as_mut().expect("server stdin");
        serde_json::to_writer(&mut *stdin, &message).expect("write request");
        stdin.write_all(b"\n").expect("write request delimiter");
        stdin.flush().expect("flush request");
        id
    }

    fn receive(&mut self, id: u64) -> Value {
        let mut line = String::new();
        assert_ne!(
            self.stdout.read_line(&mut line).expect("read response"),
            0,
            "server closed stdout before responding"
        );
        let response: Value = serde_json::from_str(&line).expect("response JSON");
        assert_eq!(response["id"], id);
        response
    }

    fn close(mut self) -> String {
        self.stdin.take();
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = self.child.try_wait().expect("poll server") {
                break status;
            }
            if Instant::now() >= deadline {
                self.child.kill().expect("kill hung server");
                panic!("server did not exit after stdin EOF");
            }
            thread::sleep(Duration::from_millis(10));
        };
        assert!(status.success(), "server exited with {status}");
        status.to_string()
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

#[derive(Clone, Copy)]
struct ResourceSample {
    memory_bytes: u64,
    resource_count: u64,
    threads: u64,
}

struct Sample {
    resources: ResourceSample,
    descendants: Vec<u32>,
}

struct Artifact {
    writer: BufWriter<File>,
    path: PathBuf,
}

impl Artifact {
    fn create() -> Self {
        let path = output_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create soak artifact directory");
        }
        let writer = BufWriter::new(File::create(&path).expect("create soak artifact"));
        Self { writer, path }
    }

    fn write(&mut self, value: &Value) {
        serde_json::to_writer(&mut self.writer, value).expect("write soak record");
        self.writer
            .write_all(b"\n")
            .expect("write record delimiter");
        self.writer.flush().expect("flush soak record");
    }
}

fn modern_meta() -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": {
            "name": "codexshim-resource-soak",
            "version": "1.0.0",
        },
        "io.modelcontextprotocol/clientCapabilities": {},
    })
}

fn output_path() -> PathBuf {
    if let Some(path) = env::var_os("CODEXSHIM_SOAK_OUTPUT") {
        return PathBuf::from(path);
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("resource-soak")
        .join(format!("{}-mixed.jsonl", env::consts::OS))
}

fn command_output(program: &str, args: &[&str]) -> String {
    match Command::new(program).args(args).output() {
        Ok(output) => String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        Err(error) => format!("unavailable: {error}"),
    }
}

fn runner_image() -> String {
    if let Ok(image) = env::var("CODEXSHIM_SOAK_RUNNER_IMAGE") {
        return image;
    }
    match (env::var("ImageOS"), env::var("ImageVersion")) {
        (Ok(os), Ok(version)) => format!("{os}-{version}"),
        _ => local_runner_image(),
    }
}

#[cfg(windows)]
fn local_runner_image() -> String {
    format!("local-{}", command_output("cmd", &["/c", "ver"]))
}

#[cfg(unix)]
fn local_runner_image() -> String {
    fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|contents| {
            contents.lines().find_map(|line| {
                line.strip_prefix("PRETTY_NAME=")
                    .map(|value| value.trim_matches('"').to_owned())
            })
        })
        .unwrap_or_else(|| format!("local-{}", env::consts::OS))
}

fn iteration_count() -> usize {
    match env::var("CODEXSHIM_SOAK_ITERATIONS") {
        Ok(value) => value
            .parse::<usize>()
            .expect("CODEXSHIM_SOAK_ITERATIONS must be a positive integer"),
        Err(_) if env::var_os("CODEXSHIM_SOAK_EXTENDED").is_some() => EXTENDED_ITERATIONS,
        Err(_) => DEFAULT_ITERATIONS,
    }
}

fn warm_up_count(iterations: usize) -> usize {
    match env::var("CODEXSHIM_SOAK_WARM_UP") {
        Ok(value) => value
            .parse::<usize>()
            .expect("CODEXSHIM_SOAK_WARM_UP must be an integer"),
        Err(_) => (iterations / 5).clamp(1, 100),
    }
}

fn run_mixed_cycle(session: &mut Session) -> Value {
    let read = session.call_tool("read", json!({ "path": "Cargo.toml", "line_count": 40 }));
    let glob = session.call_tool(
        "glob",
        json!({ "path": "src", "pattern": "**/*.rs", "limit": 100 }),
    );
    let grep = session.call_tool(
        "grep",
        json!({
            "path": "src",
            "pattern": "codexshim",
            "glob": "*.rs",
            "limit": 100,
        }),
    );
    let process = session.call_tool(
        "run_process",
        json!({
            "program": "cargo",
            "args": ["--version"],
            "cwd": env!("CARGO_MANIFEST_DIR"),
            "timeout_ms": 30_000,
        }),
    );
    let process_text = process["result"]["content"][0]["text"]
        .as_str()
        .expect("process text");
    assert!(process_text.contains("Exit code: 0"));

    json!({
        "read": outcome(&read),
        "glob": outcome(&glob),
        "grep": outcome(&grep),
        "run_process": outcome(&process),
    })
}

fn outcome(response: &Value) -> &'static str {
    if response["result"]["isError"] == false && response["result"]["resultType"] == "complete" {
        "complete"
    } else {
        "unexpected"
    }
}

fn median(mut values: Vec<u64>) -> f64 {
    values.sort_unstable();
    let middle = values.len() / 2;
    if values.len().is_multiple_of(2) {
        f64::midpoint(
            metric_as_f64(values[middle - 1]),
            metric_as_f64(values[middle]),
        )
    } else {
        metric_as_f64(values[middle])
    }
}

fn slope(values: &[u64]) -> f64 {
    let count = f64::from(u32::try_from(values.len()).expect("bounded sample count"));
    let mean_x = (count - 1.0) / 2.0;
    let mean_y = values
        .iter()
        .map(|value| metric_as_f64(*value))
        .sum::<f64>()
        / count;
    let mut numerator = 0.0;
    let mut denominator = 0.0;
    for (index, value) in values.iter().enumerate() {
        let x = f64::from(u32::try_from(index).expect("bounded sample index")) - mean_x;
        numerator += x * (metric_as_f64(*value) - mean_y);
        denominator += x * x;
    }
    if denominator == 0.0 {
        0.0
    } else {
        numerator / denominator
    }
}

fn metric_as_f64(value: u64) -> f64 {
    // Resource metrics remain far below f64's exact integer range on supported runners.
    #[allow(clippy::cast_precision_loss)]
    let value = value as f64;
    value
}

fn metric_summary(samples: &[ResourceSample], select: fn(&ResourceSample) -> u64) -> Value {
    let values = samples.iter().map(select).collect::<Vec<_>>();
    let window = (values.len() / 2).clamp(1, 10);
    json!({
        "first_window_median": median(values[..window].to_vec()),
        "last_window_median": median(values[values.len() - window..].to_vec()),
        "least_squares_slope_per_iteration": slope(&values),
        "window_size": window,
    })
}

#[test]
#[ignore = "manual resource soak; run with --ignored --nocapture"]
fn mixed_workload_resource_soak() {
    let iterations = iteration_count();
    assert!(
        iterations >= 2,
        "resource soak requires at least two iterations"
    );
    let warm_up = warm_up_count(iterations);
    assert!(
        warm_up < iterations,
        "warm-up must be shorter than the soak"
    );

    let commit = command_output("git", &["rev-parse", "HEAD"]);
    let worktree = command_output("git", &["status", "--short"]);
    let production_worktree = command_output(
        "git",
        &["status", "--short", "--", "src", "Cargo.toml", "Cargo.lock"],
    );
    let toolchain = command_output("cargo", &["--version", "--verbose"]);
    let mut artifact = Artifact::create();
    artifact.write(&json!({
        "record_type": "metadata",
        "schema_version": 1,
        "scenario": "mixed_workload",
        "platform": env::consts::OS,
        "architecture": env::consts::ARCH,
        "commit": commit,
        "worktree_status": worktree,
        "production_server_unchanged": production_worktree.is_empty(),
        "production_worktree_status": production_worktree,
        "toolchain": toolchain,
        "runner_image": runner_image(),
        "iterations": iterations,
        "warm_up_iterations": warm_up,
        "sample_unit": "one sequential read, glob, grep, and run_process cycle",
        "started_unix_ms": SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_millis(),
    }));

    let mut session = Session::start();
    session.discover();
    let mut samples = Vec::with_capacity(iterations);
    let mut surviving_descendants = BTreeMap::<usize, Vec<u32>>::new();
    for iteration in 1..=iterations {
        let started = Instant::now();
        let outcomes = run_mixed_cycle(&mut session);
        let sample = platform::sample(session.pid()).expect("sample server resources");
        if !sample.descendants.is_empty() {
            surviving_descendants.insert(iteration, sample.descendants.clone());
        }
        artifact.write(&json!({
            "record_type": "sample",
            "iteration": iteration,
            "phase": if iteration <= warm_up { "warm_up" } else { "measured" },
            "elapsed_ms": started.elapsed().as_millis(),
            "memory_kind": platform::MEMORY_KIND,
            "memory_bytes": sample.resources.memory_bytes,
            "resource_kind": platform::RESOURCE_KIND,
            "resource_count": sample.resources.resource_count,
            "threads": sample.resources.threads,
            "active_descendant_pids": sample.descendants,
            "outcomes": outcomes,
        }));
        samples.push(sample.resources);
    }

    let server_exit_status = session.close();
    let measured = &samples[warm_up..];
    artifact.write(&json!({
        "record_type": "result",
        "outcome": if surviving_descendants.is_empty() { "pass" } else { "fail" },
        "server_exit_status": server_exit_status,
        "surviving_descendants": surviving_descendants,
        "memory": metric_summary(measured, |sample| sample.memory_bytes),
        "resources": metric_summary(measured, |sample| sample.resource_count),
        "threads": metric_summary(measured, |sample| sample.threads),
        "threshold_policy": "observational growth; zero surviving controlled descendants is blocking",
        "finished_unix_ms": SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_millis(),
    }));
    assert!(
        surviving_descendants.is_empty(),
        "controlled descendants survived: {surviving_descendants:?}"
    );
    println!("resource soak artifact: {}", artifact.path.display());
}

#[test]
fn pending_process_child_fixture() {
    if env::var("CODEXSHIM_PENDING_FIXTURE").as_deref() != Ok("child") {
        return;
    }
    thread::sleep(Duration::from_secs(30));
}

#[test]
#[ignore = "manual pending-request growth probe; run with --ignored --nocapture"]
fn pending_request_growth_probe() {
    const STDIN_BYTES: usize = 1024 * 1024;
    const DEFAULT_REQUESTS: usize = 32;

    let request_count = env::var("CODEXSHIM_PENDING_REQUESTS").map_or(DEFAULT_REQUESTS, |value| {
        value
            .parse::<usize>()
            .expect("CODEXSHIM_PENDING_REQUESTS must be a positive integer")
    });
    assert!(request_count >= 4, "probe requires at least four requests");
    let output = env::var_os("CODEXSHIM_PENDING_OUTPUT").map_or_else(
        || {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("target")
                .join("resource-soak")
                .join(format!("{}-pending.jsonl", env::consts::OS))
        },
        PathBuf::from,
    );
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent).expect("create pending artifact directory");
    }
    let mut artifact = Artifact {
        writer: BufWriter::new(File::create(&output).expect("create pending artifact")),
        path: output,
    };
    artifact.write(&json!({
        "record_type": "metadata",
        "schema_version": 1,
        "scenario": "pending_process_request_growth",
        "platform": env::consts::OS,
        "architecture": env::consts::ARCH,
        "commit": command_output("git", &["rev-parse", "HEAD"]),
        "worktree_status": command_output("git", &["status", "--short"]),
        "toolchain": command_output("cargo", &["--version", "--verbose"]),
        "runner_image": runner_image(),
        "request_count": request_count,
        "encoded_payload_bytes_per_request": STDIN_BYTES,
        "active_process_limit": 2,
        "admission_mode": "class_aware_fail_fast_8_2",
        "started_unix_ms": SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_millis(),
    }));

    let mut session = Session::start();
    session.discover();
    let executable = env::current_exe().expect("integration test executable");
    let payload = "p".repeat(STDIN_BYTES);
    let mut samples = Vec::new();
    let baseline = platform::sample(session.pid()).expect("sample pending baseline");
    artifact.write(&pending_sample(0, &baseline));
    samples.push((0_usize, baseline.resources.memory_bytes));

    let checkpoints = [
        request_count / 4,
        request_count / 2,
        request_count.saturating_mul(3) / 4,
        request_count,
    ];
    let mut sent = 0_usize;
    for checkpoint in checkpoints {
        while sent < checkpoint {
            session.send_tool(
                "run_process",
                json!({
                    "program": executable,
                    "args": ["--exact", "pending_process_child_fixture", "--nocapture"],
                    "cwd": env!("CARGO_MANIFEST_DIR"),
                    "env": { "CODEXSHIM_PENDING_FIXTURE": "child" },
                    "stdin": payload,
                    "timeout_ms": 30_000,
                }),
            );
            sent += 1;
        }
        thread::sleep(Duration::from_millis(300));
        let sample = platform::sample(session.pid()).expect("sample pending requests");
        artifact.write(&pending_sample(sent, &sample));
        samples.push((sent, sample.resources.memory_bytes));
    }

    let initial_memory = samples.first().expect("initial sample").1;
    let final_memory = samples.last().expect("final sample").1;
    let slope = request_slope(&samples);
    artifact.write(&json!({
        "record_type": "result",
        "outcome": "measured",
        "initial_memory_bytes": initial_memory,
        "final_memory_bytes": final_memory,
        "retained_growth_bytes": final_memory.saturating_sub(initial_memory),
        "least_squares_bytes_per_queued_request": slope,
        "threshold_policy": "adoption evidence only; not an absolute RSS regression gate",
        "finished_unix_ms": SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_millis(),
    }));
    session.stdin.take();
    session.close();
    println!("pending request artifact: {}", artifact.path.display());
}

fn pending_sample(request_count: usize, sample: &Sample) -> Value {
    json!({
        "record_type": "sample",
        "queued_request_count": request_count,
        "memory_kind": platform::MEMORY_KIND,
        "memory_bytes": sample.resources.memory_bytes,
        "resource_kind": platform::RESOURCE_KIND,
        "resource_count": sample.resources.resource_count,
        "threads": sample.resources.threads,
        "active_descendant_pids": sample.descendants,
    })
}

fn request_slope(samples: &[(usize, u64)]) -> f64 {
    let count = f64::from(u32::try_from(samples.len()).expect("bounded sample count"));
    let mean_x = samples
        .iter()
        .map(|(requests, _)| metric_as_f64(u64::try_from(*requests).expect("request count")))
        .sum::<f64>()
        / count;
    let mean_y = samples
        .iter()
        .map(|(_, memory)| metric_as_f64(*memory))
        .sum::<f64>()
        / count;
    let (numerator, denominator) = samples.iter().fold(
        (0.0, 0.0),
        |(numerator, denominator), (requests, memory)| {
            let x = metric_as_f64(u64::try_from(*requests).expect("request count")) - mean_x;
            (
                numerator + x * (metric_as_f64(*memory) - mean_y),
                denominator + x * x,
            )
        },
    );
    if denominator == 0.0 {
        0.0
    } else {
        numerator / denominator
    }
}

#[cfg(unix)]
mod platform {
    use std::{collections::BTreeMap, fs, io};

    use super::{ResourceSample, Sample};

    pub const MEMORY_KIND: &str = "rss";
    pub const RESOURCE_KIND: &str = "file_descriptors";

    pub fn sample(server_pid: u32) -> io::Result<Sample> {
        let status = fs::read_to_string(format!("/proc/{server_pid}/status"))?;
        let memory_bytes = status_value(&status, "VmRSS:")? * 1_024;
        let threads = status_value(&status, "Threads:")?;
        let resource_count = fs::read_dir(format!("/proc/{server_pid}/fd"))?.count() as u64;
        let descendants = descendant_pids(server_pid)?;
        Ok(Sample {
            resources: ResourceSample {
                memory_bytes,
                resource_count,
                threads,
            },
            descendants,
        })
    }

    fn status_value(status: &str, name: &str) -> io::Result<u64> {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|value| value.split_whitespace().next())
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("missing {name}")))
    }

    fn descendant_pids(server_pid: u32) -> io::Result<Vec<u32>> {
        let mut children = BTreeMap::<u32, Vec<u32>>::new();
        for entry in fs::read_dir("/proc")? {
            let entry = entry?;
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse().ok())
            else {
                continue;
            };
            let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
                continue;
            };
            let Some(parent_pid) = parent_pid(&stat) else {
                continue;
            };
            children.entry(parent_pid).or_default().push(pid);
        }
        let mut descendants = Vec::new();
        let mut pending = vec![server_pid];
        while let Some(parent) = pending.pop() {
            if let Some(direct) = children.get(&parent) {
                descendants.extend(direct);
                pending.extend(direct);
            }
        }
        descendants.sort_unstable();
        Ok(descendants)
    }

    fn parent_pid(stat: &str) -> Option<u32> {
        let after_name = stat.rsplit_once(") ")?.1;
        after_name.split_whitespace().nth(1)?.parse().ok()
    }
}

#[cfg(windows)]
mod platform {
    use std::{collections::BTreeMap, io, mem};

    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE},
        System::Threading::{
            GetProcessHandleCount, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
        },
    };

    use super::{ResourceSample, Sample};

    const TH32CS_SNAPPROCESS: u32 = 0x0000_0002;
    pub const MEMORY_KIND: &str = "working_set";
    pub const RESOURCE_KIND: &str = "handles";

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
        fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> HANDLE;
        fn Process32FirstW(snapshot: HANDLE, entry: *mut ProcessEntry32W) -> i32;
        fn Process32NextW(snapshot: HANDLE, entry: *mut ProcessEntry32W) -> i32;
    }

    pub fn sample(server_pid: u32) -> io::Result<Sample> {
        // SAFETY: The PID comes from a live Child and the returned handle is checked and closed.
        let process = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ,
                0,
                server_pid,
            )
        };
        if process.is_null() {
            return Err(io::Error::last_os_error());
        }
        let result = (|| {
            let (memory_bytes, resource_count) = sample_process(process)?;
            let threads = process_entries()?
                .into_iter()
                .find_map(|(pid, _, threads)| (pid == server_pid).then_some(u64::from(threads)))
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "server process"))?;
            Ok(Sample {
                resources: ResourceSample {
                    memory_bytes,
                    resource_count,
                    threads,
                },
                descendants: descendant_pids(server_pid)?,
            })
        })();
        // SAFETY: process is a valid owned handle returned by OpenProcess.
        unsafe { CloseHandle(process) };
        result
    }

    fn sample_process(process: HANDLE) -> io::Result<(u64, u64)> {
        // SAFETY: The zeroed C structure is initialized with its size before the API call.
        let mut counters: ProcessMemoryCounters = unsafe { mem::zeroed() };
        counters.cb =
            u32::try_from(mem::size_of::<ProcessMemoryCounters>()).expect("counter structure size");
        // SAFETY: process is valid and counters points to writable memory of the declared size.
        let memory_ok = unsafe { K32GetProcessMemoryInfo(process, &raw mut counters, counters.cb) };
        if memory_ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut handles = 0_u32;
        // SAFETY: process is valid and handles points to a writable u32.
        if unsafe { GetProcessHandleCount(process, &raw mut handles) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((counters.working_set_size as u64, u64::from(handles)))
    }

    fn process_entries() -> io::Result<Vec<(u32, u32, u32)>> {
        // SAFETY: The API has no borrowed pointer arguments and the handle is checked and closed.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: The zeroed C structure receives its required size before enumeration.
        let mut entry: ProcessEntry32W = unsafe { mem::zeroed() };
        entry.size =
            u32::try_from(mem::size_of::<ProcessEntry32W>()).expect("process entry structure size");
        let mut entries = Vec::new();
        // SAFETY: snapshot and entry are valid for the duration of enumeration.
        let mut has_entry = unsafe { Process32FirstW(snapshot, &raw mut entry) } != 0;
        while has_entry {
            entries.push((entry.process_id, entry.parent_process_id, entry.threads));
            // SAFETY: snapshot and entry remain valid for the next enumeration call.
            has_entry = unsafe { Process32NextW(snapshot, &raw mut entry) } != 0;
        }
        // SAFETY: snapshot is a valid owned handle.
        unsafe { CloseHandle(snapshot) };
        Ok(entries)
    }

    fn descendant_pids(server_pid: u32) -> io::Result<Vec<u32>> {
        let mut children = BTreeMap::<u32, Vec<u32>>::new();
        for (pid, parent_pid, _) in process_entries()? {
            children.entry(parent_pid).or_default().push(pid);
        }
        let mut descendants = Vec::new();
        let mut pending = vec![server_pid];
        while let Some(parent) = pending.pop() {
            if let Some(direct) = children.get(&parent) {
                descendants.extend(direct);
                pending.extend(direct);
            }
        }
        descendants.sort_unstable();
        Ok(descendants)
    }
}
