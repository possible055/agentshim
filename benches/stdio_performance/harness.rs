use super::{
    Arc, AtomicBool, BTreeSet, BufRead, BufReader, Child, ChildStdin, ChildStdout, Command,
    Duration, Instant, Ordering, Path, Stdio, Value, Write, json,
};

pub(super) struct Session {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl Session {
    pub(super) fn start(mode: &str, logs: &std::path::Path) -> Self {
        Self::start_in(mode, logs, Path::new(env!("CARGO_MANIFEST_DIR")))
    }

    pub(super) fn start_in(mode: &str, logs: &Path, root: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_agentshim"))
            .arg("serve")
            .current_dir(root)
            .env("AGENTSHIM_LOG_MODE", mode)
            .env("AGENTSHIM_LOG_DIR", logs)
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

    pub(super) fn pid(&self) -> u32 {
        self.child.id()
    }

    pub(super) fn read(&mut self, id: u64) -> Value {
        self.read_path(id, "Cargo.toml")
    }

    pub(super) fn read_path(&mut self, id: u64, path: &str) -> Value {
        self.send_read(id, path);
        let response = self.receive_tool_response("read");
        assert_eq!(response["id"], id);
        response["result"].clone()
    }

    fn send_read(&mut self, id: u64, path: &str) {
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {
                "name": "read",
                "arguments": { "path": path, "line_count": 1000 },
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
    }

    pub(super) fn grep(&mut self, id: u64, glob: &str, mode: &str) -> Value {
        self.send_grep(id, glob, mode);
        self.receive_tool(id, "grep")
    }

    pub(super) fn grep_batch(
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

    pub(super) fn send_grep(&mut self, id: u64, glob: &str, mode: &str) {
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

    pub(super) fn mixed_grep_and_read(
        &mut self,
        grep_id: u64,
        read_id: u64,
        glob: &str,
        mode: &str,
        expected_grep: &Value,
    ) -> (f64, f64) {
        let started = Instant::now();
        self.send_grep(grep_id, glob, mode);
        self.send_read(read_id, "templates/ordinary-000000.rs");
        let mut grep_ms = None;
        let mut read_ms = None;
        for _ in 0..2 {
            let response = self.receive_tool_response("mixed grep/read");
            let id = response["id"].as_u64().expect("mixed response id");
            if id == grep_id {
                assert_eq!(
                    response["result"], *expected_grep,
                    "mixed grep output changed"
                );
                grep_ms = Some(started.elapsed().as_secs_f64() * 1_000.0);
            } else if id == read_id {
                assert_eq!(response["result"]["isError"], false, "mixed read failed");
                read_ms = Some(started.elapsed().as_secs_f64() * 1_000.0);
            } else {
                panic!("unexpected mixed response {id}");
            }
        }
        (
            read_ms.expect("read completed"),
            grep_ms.expect("grep completed"),
        )
    }

    pub(super) fn run_process(&mut self, id: u64) -> Value {
        self.send_process(id);
        self.receive_process(id)
    }

    pub(super) fn run_process_batch(&mut self, first_id: u64, calls: usize) -> Vec<f64> {
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

    pub(super) fn send_process(&mut self, id: u64) {
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {
                "name": "run_program",
                "arguments": {
                    "program": env!("CARGO_BIN_EXE_agentshim"),
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

    pub(super) fn receive_process(&mut self, id: u64) -> Value {
        self.receive_tool(id, "process")
    }

    pub(super) fn receive_process_response(&mut self) -> Value {
        self.receive_tool_response("process")
    }

    pub(super) fn receive_tool(&mut self, id: u64, tool: &str) -> Value {
        let response = self.receive_tool_response(tool);
        assert_eq!(response["id"], id);
        assert_eq!(
            response["result"]["isError"], false,
            "{tool} call failed: {response}"
        );
        response["result"].clone()
    }

    pub(super) fn receive_tool_response(&mut self, tool: &str) -> Value {
        let mut response = String::new();
        self.stdout
            .read_line(&mut response)
            .unwrap_or_else(|error| panic!("{tool} response: {error}"));
        serde_json::from_str(&response)
            .unwrap_or_else(|error| panic!("{tool} JSON-RPC response: {error}"))
    }

    pub(super) fn close(mut self) {
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
pub(super) struct ResourceHighWater {
    pub(super) peak_working_set_bytes: u64,
    pub(super) peak_threads: u64,
    pub(super) peak_handles: u64,
    pub(super) read_operation_delta: u64,
    pub(super) read_bytes_delta: u64,
    pub(super) write_operation_delta: u64,
    pub(super) write_bytes_delta: u64,
    pub(super) page_fault_delta: u64,
}

pub(super) struct ResourceMonitor {
    stop: Arc<AtomicBool>,
    worker: std::thread::JoinHandle<ResourceHighWater>,
}

impl ResourceMonitor {
    pub(super) fn start(pid: u32) -> Self {
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

    pub(super) fn finish(self) -> ResourceHighWater {
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

    pub(super) fn process_threads(pid: u32) -> io::Result<u32> {
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

    pub(super) fn status_value(status: &str, key: &str) -> io::Result<u64> {
        status
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .and_then(|value| value.split_whitespace().next())
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, key))
    }

    pub(super) fn io_value(contents: &str, key: &str) -> io::Result<u64> {
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
