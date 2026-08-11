use super::*;

pub(super) struct Session {
    child: Child,
    pub(super) stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Session {
    pub(super) fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_codexshim"))
            .arg("serve")
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .env_remove("CODEXSHIM_MCP_COMPATIBILITY")
            .env_remove("CODEXSHIM_PROCESS_CALLS")
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

    pub(super) fn pid(&self) -> u32 {
        self.child.id()
    }

    pub(super) fn discover(&mut self) {
        let response = self.request("server/discover", Map::new());
        assert_eq!(response["result"]["capabilities"], json!({ "tools": {} }));
    }

    pub(super) fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
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

    pub(super) fn send_tool(&mut self, name: &str, arguments: Value) -> u64 {
        let mut params = Map::new();
        params.insert("name".to_owned(), json!(name));
        params.insert("arguments".to_owned(), arguments);
        self.send_request("tools/call", params)
    }

    pub(super) fn request(&mut self, method: &str, params: Map<String, Value>) -> Value {
        let id = self.send_request(method, params);
        self.receive(id)
    }

    pub(super) fn send_request(&mut self, method: &str, mut params: Map<String, Value>) -> u64 {
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

    pub(super) fn receive(&mut self, id: u64) -> Value {
        let response = self.receive_any();
        assert_eq!(response["id"], id);
        response
    }

    pub(super) fn receive_any(&mut self) -> Value {
        let mut line = String::new();
        assert_ne!(
            self.stdout.read_line(&mut line).expect("read response"),
            0,
            "server closed stdout before responding"
        );
        serde_json::from_str(&line).expect("response JSON")
    }

    pub(super) fn close(mut self) -> String {
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
pub(super) struct ResourceSample {
    pub(super) memory_bytes: u64,
    pub(super) virtual_memory_bytes: Option<u64>,
    pub(super) resource_count: u64,
    pub(super) threads: u64,
}

pub(super) struct Sample {
    pub(super) resources: ResourceSample,
    pub(super) descendants: Vec<u32>,
}

pub(super) struct Artifact {
    pub(super) writer: BufWriter<File>,
    pub(super) path: PathBuf,
}

impl Artifact {
    pub(super) fn create() -> Self {
        let path = output_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create soak artifact directory");
        }
        let writer = BufWriter::new(File::create(&path).expect("create soak artifact"));
        Self { writer, path }
    }

    pub(super) fn write(&mut self, value: &Value) {
        serde_json::to_writer(&mut self.writer, value).expect("write soak record");
        self.writer
            .write_all(b"\n")
            .expect("write record delimiter");
        self.writer.flush().expect("flush soak record");
    }
}

pub(super) fn modern_meta() -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": {
            "name": "codexshim-resource-soak",
            "version": "1.0.0",
        },
        "io.modelcontextprotocol/clientCapabilities": {},
    })
}

pub(super) fn output_path() -> PathBuf {
    if let Some(path) = env::var_os("CODEXSHIM_SOAK_OUTPUT") {
        return PathBuf::from(path);
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("resource-soak")
        .join(format!("{}-mixed.jsonl", env::consts::OS))
}

pub(super) fn command_output(program: &str, args: &[&str]) -> String {
    match Command::new(program).args(args).output() {
        Ok(output) => String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        Err(error) => format!("unavailable: {error}"),
    }
}

pub(super) fn runner_image() -> String {
    if let Ok(image) = env::var("CODEXSHIM_SOAK_RUNNER_IMAGE") {
        return image;
    }
    match (env::var("ImageOS"), env::var("ImageVersion")) {
        (Ok(os), Ok(version)) => format!("{os}-{version}"),
        _ => local_runner_image(),
    }
}

#[cfg(windows)]
pub(super) fn local_runner_image() -> String {
    format!("local-{}", command_output("cmd", &["/c", "ver"]))
}

#[cfg(unix)]
pub(super) fn local_runner_image() -> String {
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

pub(super) fn iteration_count() -> usize {
    match env::var("CODEXSHIM_SOAK_ITERATIONS") {
        Ok(value) => value
            .parse::<usize>()
            .expect("CODEXSHIM_SOAK_ITERATIONS must be a positive integer"),
        Err(_) if env::var_os("CODEXSHIM_SOAK_EXTENDED").is_some() => EXTENDED_ITERATIONS,
        Err(_) => DEFAULT_ITERATIONS,
    }
}

pub(super) fn warm_up_count(iterations: usize) -> usize {
    match env::var("CODEXSHIM_SOAK_WARM_UP") {
        Ok(value) => value
            .parse::<usize>()
            .expect("CODEXSHIM_SOAK_WARM_UP must be an integer"),
        Err(_) => (iterations / 5).clamp(1, 100),
    }
}

pub(super) fn run_mixed_cycle(session: &mut Session) -> Value {
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
        "run_program",
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
        "run_program": outcome(&process),
    })
}

pub(super) fn outcome(response: &Value) -> &'static str {
    if response["result"]["isError"] == false && response["result"]["resultType"] == "complete" {
        "complete"
    } else {
        "unexpected"
    }
}

pub(super) fn median(mut values: Vec<u64>) -> f64 {
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

pub(super) fn slope(values: &[u64]) -> f64 {
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

pub(super) fn metric_as_f64(value: u64) -> f64 {
    // Resource metrics remain far below f64's exact integer range on supported runners.
    #[allow(clippy::cast_precision_loss)]
    let value = value as f64;
    value
}

pub(super) fn metric_summary(
    samples: &[ResourceSample],
    select: fn(&ResourceSample) -> u64,
) -> Value {
    let values = samples.iter().map(select).collect::<Vec<_>>();
    let window = (values.len() / 2).clamp(1, 10);
    json!({
        "first_window_median": median(values[..window].to_vec()),
        "last_window_median": median(values[values.len() - window..].to_vec()),
        "least_squares_slope_per_iteration": slope(&values),
        "window_size": window,
    })
}

#[cfg(unix)]
pub(super) mod platform {
    use std::{collections::BTreeMap, fs, io};

    use super::{ResourceSample, Sample};

    pub const MEMORY_KIND: &str = "rss";
    pub const RESOURCE_KIND: &str = "file_descriptors";

    pub fn sample(server_pid: u32) -> io::Result<Sample> {
        let status = fs::read_to_string(format!("/proc/{server_pid}/status"))?;
        let memory_bytes = status_value(&status, "VmRSS:")? * 1_024;
        let virtual_memory_bytes = status_value(&status, "VmSize:")? * 1_024;
        let threads = status_value(&status, "Threads:")?;
        let resource_count = fs::read_dir(format!("/proc/{server_pid}/fd"))?.count() as u64;
        let descendants = descendant_pids(server_pid)?;
        Ok(Sample {
            resources: ResourceSample {
                memory_bytes,
                virtual_memory_bytes: Some(virtual_memory_bytes),
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
pub(super) mod platform {
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
                    virtual_memory_bytes: None,
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
