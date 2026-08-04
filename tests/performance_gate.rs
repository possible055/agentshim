use std::{
    fs,
    path::Path,
    process::Command,
    sync::{Arc, Barrier},
    time::{Duration, Instant},
};

use codexshim::{
    path::RepositoryRoot,
    runtime::RuntimeConfig,
    tools::{
        glob::{self, GlobError, GlobRequest},
        grep::{self, GrepError, GrepMode, GrepRequest},
        read::{self, ReadRequest},
    },
    traversal::TraversalError,
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

const SAMPLES: usize = 7;

struct Corpus {
    fixture: tempfile::TempDir,
    root: Arc<RepositoryRoot>,
    files: usize,
    large_file: String,
    searchable_bytes: usize,
    read_bytes: usize,
}

impl Corpus {
    fn create(files: usize) -> Self {
        let fixture = tempfile::tempdir().expect("performance fixture");
        let root_path = fixture.path();
        let mut searchable_bytes = 0_usize;
        for directory in ["many", "wide", "deep", ".hidden", "ignored", ".git"] {
            fs::create_dir_all(root_path.join(directory)).expect("corpus directory");
        }
        fs::write(root_path.join(".gitignore"), "ignored/\n").expect("root ignore");
        fs::write(root_path.join(".ignore"), "**/*.skip\n").expect("root .ignore");
        fs::write(root_path.join(".git/config"), "must never be traversed\n").expect("git fixture");

        for index in 0..files {
            let (directory, extension) = match index % 60 {
                0 => (root_path.join("wide"), "rs"),
                20 => (root_path.join(format!("many/{:03}", index % 128)), "rs"),
                40 => (
                    root_path.join(format!("deep/a/b/c/d/e/{:03}", index % 64)),
                    "rs",
                ),
                1 => (root_path.join(".hidden"), "txt"),
                2 => (root_path.join("ignored"), "txt"),
                3 => (root_path.join("many"), "skip"),
                _ => (root_path.join(format!("many/{:03}", index % 128)), "txt"),
            };
            fs::create_dir_all(&directory).expect("corpus shard");
            let path = directory.join(format!("file-{index:07}.{extension}"));
            let content = if extension == "rs" {
                format!("fn item_{index}() {{ let needle = {index}; }}\n")
            } else {
                format!("ordinary corpus record {index}\n")
            };
            if extension == "rs" {
                searchable_bytes = searchable_bytes.saturating_add(content.len());
            }
            fs::write(path, content).expect("corpus file");
        }

        let unicode = "fn unicode_needle() {}\n";
        fs::write(root_path.join("unicode-界.rs"), unicode).expect("Unicode fixture");
        searchable_bytes = searchable_bytes.saturating_add(unicode.len());
        fs::write(root_path.join("utf16.txt"), utf16_le("UTF-16 needle\n"))
            .expect("UTF-16 fixture");
        fs::write(root_path.join("binary.bin"), b"\0binary\0fixture").expect("binary fixture");
        let large = root_path.join("large.txt");
        let large_content = "large needle line\n".repeat(500_000);
        let read_bytes = "large needle line\n".len().saturating_mul(2_000);
        fs::write(&large, large_content).expect("large fixture");
        let root = Arc::new(RepositoryRoot::open(root_path).expect("open corpus root"));
        Self {
            fixture,
            root,
            files: files + 4,
            large_file: large.to_string_lossy().into_owned(),
            searchable_bytes,
            read_bytes,
        }
    }

    fn path(&self) -> &Path {
        self.fixture.path()
    }
}

fn utf16_le(text: &str) -> Vec<u8> {
    let mut bytes = vec![0xFF, 0xFE];
    for unit in text.encode_utf16() {
        bytes.extend(unit.to_le_bytes());
    }
    bytes
}

fn benchmark(files: usize) {
    let corpus = Corpus::create(files);
    let cancellation = CancellationToken::new();
    let default_lanes = RuntimeConfig::from_env()
        .expect("runtime benchmark configuration")
        .worker_lanes;
    let glob_request = GlobRequest {
        pattern: "**/*.rs".to_owned(),
        path: None,
        include_ignored: None,
        offset: None,
        limit: Some(1_000),
    };
    let grep_request = GrepRequest {
        pattern: "needle".to_owned(),
        path: None,
        glob: Some("**/*.rs".to_owned()),
        mode: Some(GrepMode::Count),
        fixed_strings: Some(true),
        case: None,
        context_lines: None,
        offset: None,
        limit: Some(1_000),
    };
    let read_request = ReadRequest {
        path: corpus.large_file.clone(),
        start_line: Some(1),
        line_count: Some(2_000),
        encoding: None,
    };

    let glob_output = measure("glob", corpus.files, 0, || {
        glob::execute(&corpus.root, &glob_request, &cancellation).expect("glob benchmark")
    });
    let grep_output = measure("grep", corpus.files, corpus.searchable_bytes, || {
        grep::execute(&corpus.root, &grep_request, default_lanes, &cancellation)
            .expect("grep benchmark")
    });
    measure_grep_lanes(
        &corpus,
        &grep_request,
        &grep_output,
        default_lanes,
        &cancellation,
    );
    measure("read", 1, corpus.read_bytes, || {
        read::execute(&corpus.root, &read_request, &cancellation).expect("read benchmark")
    });
    compare_ripgrep(&corpus, &glob_output, &grep_output);
    for concurrency in [1_usize, 4, 8] {
        measure_concurrency(&corpus.root, &glob_request, corpus.files, concurrency);
    }
    if files >= 100_000 {
        measure_cancellation("glob", || {
            let root = Arc::clone(&corpus.root);
            let request = glob_request.clone();
            move |cancellation| {
                matches!(
                    glob::execute(&root, &request, cancellation),
                    Err(GlobError::Traversal(TraversalError::Cancelled))
                )
            }
        });
        measure_cancellation("grep", || {
            let root = Arc::clone(&corpus.root);
            let request = grep_request.clone();
            move |cancellation| {
                matches!(
                    grep::execute(&root, &request, default_lanes, cancellation),
                    Err(GrepError::Cancelled)
                )
            }
        });
    }
}

fn measure_grep_lanes(
    corpus: &Corpus,
    request: &GrepRequest,
    reference: &str,
    default_lanes: usize,
    cancellation: &CancellationToken,
) {
    let mut variants = Vec::new();
    for lanes in [1_usize, 2, 4, 8, 16] {
        let mut samples = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            let mut execute = || {
                grep::execute(&corpus.root, request, lanes, cancellation)
                    .expect("grep lane benchmark")
            };
            let (output, sample) = measure_once(&mut execute);
            assert_eq!(reference, output, "grep lane output changed");
            samples.push(sample);
        }
        print_metrics(
            &format!("grep-{lanes}-lanes-warm"),
            corpus.files,
            corpus.searchable_bytes,
            1,
            &samples,
            None,
        );
        let mut milliseconds = samples
            .iter()
            .map(|sample| sample.duration.as_secs_f64() * 1_000.0)
            .collect::<Vec<_>>();
        milliseconds.sort_by(f64::total_cmp);
        variants.push((lanes, percentile(&milliseconds, 95)));
    }
    let fastest = variants
        .iter()
        .map(|(_, p95)| *p95)
        .min_by(f64::total_cmp)
        .expect("grep lane variants");
    let default = variants
        .iter()
        .find_map(|(lanes, p95)| (*lanes == default_lanes).then_some(*p95))
        .expect("default grep lane variant");
    println!(
        "{}",
        json!({
            "fastest_correct_p95_ms": fastest,
            "default_lanes": default_lanes,
            "operation": "grep-lane-gate",
            "variants": variants.iter().map(|(lanes, p95)| json!({ "lanes": lanes, "p95_ms": p95 })).collect::<Vec<_>>(),
        })
    );
    if corpus.files >= 100_000 {
        assert!(
            default <= fastest * 1.15,
            "default grep lanes exceed the fastest-correct p95 by more than 15%"
        );
    }
}

fn measure(
    operation: &str,
    files: usize,
    bytes: usize,
    mut execute: impl FnMut() -> String,
) -> String {
    let (reference, coldish) = measure_once(&mut execute);
    print_metrics(
        &format!("{operation}-coldish"),
        files,
        bytes,
        1,
        &[coldish],
        None,
    );

    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let (output, sample) = measure_once(&mut execute);
        samples.push(sample);
        assert_eq!(reference, output, "benchmark output changed");
    }
    print_metrics(
        &format!("{operation}-warm"),
        files,
        bytes,
        1,
        &samples,
        None,
    );
    reference
}

fn measure_concurrency(
    root: &Arc<RepositoryRoot>,
    request: &GlobRequest,
    files: usize,
    concurrency: usize,
) {
    let mut samples = Vec::with_capacity(SAMPLES);
    let mut call_durations = Vec::with_capacity(SAMPLES.saturating_mul(concurrency));
    let mut reference = None;
    for _ in 0..SAMPLES {
        let before = ResourceSnapshot::capture();
        let started = Instant::now();
        let mut workers = Vec::new();
        for _ in 0..concurrency {
            let root = root.clone();
            let request = request.clone();
            workers.push(std::thread::spawn(move || {
                let started = Instant::now();
                let output = glob::execute(&root, &request, &CancellationToken::new())
                    .expect("parallel glob");
                (output, started.elapsed())
            }));
        }
        for worker in workers {
            let (output, duration) = worker.join().expect("parallel benchmark worker");
            call_durations.push(duration);
            if let Some(reference) = &reference {
                assert_eq!(reference, &output, "parallel output changed");
            } else {
                reference = Some(output);
            }
        }
        samples.push(Measurement {
            duration: started.elapsed(),
            resources: before.delta(ResourceSnapshot::capture()),
        });
    }
    print_metrics(
        "glob-concurrent-warm",
        files,
        0,
        concurrency,
        &samples,
        Some(&call_durations),
    );
}

fn measure_cancellation<F>(operation: &str, build: impl Fn() -> F)
where
    F: FnOnce(&CancellationToken) -> bool + Send,
{
    let mut milliseconds = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let cancellation = CancellationToken::new();
        let started = Arc::new(Barrier::new(2));
        let cancelled = std::thread::scope(|scope| {
            let worker_started = Arc::clone(&started);
            let worker_cancellation = cancellation.clone();
            let execute = build();
            let worker = scope.spawn(move || {
                worker_started.wait();
                execute(&worker_cancellation)
            });
            started.wait();
            std::thread::sleep(Duration::from_millis(10));
            let cancel_started = Instant::now();
            cancellation.cancel();
            let cancelled = worker.join().expect("cancellation worker");
            milliseconds.push(cancel_started.elapsed().as_secs_f64() * 1_000.0);
            cancelled
        });
        assert!(
            cancelled,
            "{operation} completed before observing cancellation"
        );
    }
    milliseconds.sort_by(f64::total_cmp);
    let p95 = percentile(&milliseconds, 95);
    println!(
        "{}",
        json!({
            "operation": format!("{operation}-cancellation"),
            "p50_ms": percentile(&milliseconds, 50),
            "p95_ms": p95,
            "p99_ms": percentile(&milliseconds, 99),
            "samples_ms": milliseconds,
        })
    );
    assert!(p95 <= 250.0, "{operation} cancellation p95 exceeded 250 ms");
}

#[derive(Clone, Copy)]
struct Measurement {
    duration: Duration,
    resources: ResourceDelta,
}

fn measure_once(execute: &mut impl FnMut() -> String) -> (String, Measurement) {
    let before = ResourceSnapshot::capture();
    let started = Instant::now();
    let output = execute();
    let duration = started.elapsed();
    let resources = before.delta(ResourceSnapshot::capture());
    (
        output,
        Measurement {
            duration,
            resources,
        },
    )
}

fn print_metrics(
    operation: &str,
    files: usize,
    bytes: usize,
    concurrency: usize,
    samples: &[Measurement],
    call_durations: Option<&[Duration]>,
) {
    let mut milliseconds = samples
        .iter()
        .map(|sample| sample.duration.as_secs_f64() * 1_000.0)
        .collect::<Vec<_>>();
    milliseconds.sort_by(f64::total_cmp);
    let p50_seconds = percentile(&milliseconds, 50) / 1_000.0;
    let files_per_sample = f64::from(u32::try_from(files).expect("benchmark file count fits u32"));
    let bytes_per_sample = f64::from(u32::try_from(bytes).expect("benchmark byte count fits u32"));
    let concurrency_float =
        f64::from(u32::try_from(concurrency).expect("benchmark concurrency fits u32"));
    let mut call_milliseconds = call_durations.map(|durations| {
        durations
            .iter()
            .map(|duration| duration.as_secs_f64() * 1_000.0)
            .collect::<Vec<_>>()
    });
    if let Some(call_milliseconds) = &mut call_milliseconds {
        call_milliseconds.sort_by(f64::total_cmp);
    }
    println!(
        "{}",
        json!({
            "concurrency": concurrency,
            "context_switches": samples.iter().map(|sample| sample.resources.context_switches).collect::<Vec<_>>(),
            "cpu_micros": samples.iter().map(|sample| sample.resources.cpu_micros).collect::<Vec<_>>(),
            "files": files,
            "calls_per_second_p50": concurrency_float / p50_seconds,
            "call_p50_ms": call_milliseconds.as_deref().map(|values| percentile(values, 50)),
            "call_p95_ms": call_milliseconds.as_deref().map(|values| percentile(values, 95)),
            "call_p99_ms": call_milliseconds.as_deref().map(|values| percentile(values, 99)),
            "files_per_second_p50": files_per_sample * concurrency_float / p50_seconds,
            "handles_or_fds_after": samples.iter().map(|sample| sample.resources.handles_or_fds_after).collect::<Vec<_>>(),
            "io_bytes": samples.iter().map(|sample| sample.resources.io_bytes).collect::<Vec<_>>(),
            "io_operations": samples.iter().map(|sample| sample.resources.io_operations).collect::<Vec<_>>(),
            "mib_per_second_p50": (bytes > 0).then(|| bytes_per_sample / (1024.0 * 1024.0) / p50_seconds),
            "operation": operation,
            "p50_ms": percentile(&milliseconds, 50),
            "p95_ms": percentile(&milliseconds, 95),
            "p99_ms": percentile(&milliseconds, 99),
            "peak_rss_bytes": samples.iter().map(|sample| sample.resources.peak_rss_bytes).collect::<Vec<_>>(),
            "private_bytes_after": samples.iter().map(|sample| sample.resources.private_bytes_after).collect::<Vec<_>>(),
            "rss_bytes_after": samples.iter().map(|sample| sample.resources.rss_bytes_after).collect::<Vec<_>>(),
            "samples_ms": milliseconds,
            "threads_after": samples.iter().map(|sample| sample.resources.threads_after).collect::<Vec<_>>(),
        })
    );
}

fn percentile(sorted: &[f64], numerator: usize) -> f64 {
    let index = (sorted.len() * numerator).div_ceil(100).saturating_sub(1);
    sorted[index]
}

#[derive(Clone, Copy, Default)]
struct ResourceSnapshot {
    context_switches: Option<u64>,
    cpu_micros: Option<u64>,
    handles_or_fds: Option<u64>,
    io_bytes: Option<u64>,
    io_operations: Option<u64>,
    peak_rss_bytes: Option<u64>,
    private_bytes: Option<u64>,
    rss_bytes: Option<u64>,
    threads: Option<u64>,
}

#[derive(Clone, Copy)]
struct ResourceDelta {
    context_switches: Option<u64>,
    cpu_micros: Option<u64>,
    handles_or_fds_after: Option<u64>,
    io_bytes: Option<u64>,
    io_operations: Option<u64>,
    peak_rss_bytes: Option<u64>,
    private_bytes_after: Option<u64>,
    rss_bytes_after: Option<u64>,
    threads_after: Option<u64>,
}

impl ResourceSnapshot {
    fn capture() -> Self {
        platform_resource_snapshot()
    }

    fn delta(self, after: Self) -> ResourceDelta {
        ResourceDelta {
            context_switches: option_delta(self.context_switches, after.context_switches),
            cpu_micros: option_delta(self.cpu_micros, after.cpu_micros),
            handles_or_fds_after: after.handles_or_fds,
            io_bytes: option_delta(self.io_bytes, after.io_bytes),
            io_operations: option_delta(self.io_operations, after.io_operations),
            peak_rss_bytes: after.peak_rss_bytes,
            private_bytes_after: after.private_bytes,
            rss_bytes_after: after.rss_bytes,
            threads_after: after.threads,
        }
    }
}

fn option_delta(before: Option<u64>, after: Option<u64>) -> Option<u64> {
    before
        .zip(after)
        .map(|(before, after)| after.saturating_sub(before))
}

#[cfg(target_os = "linux")]
fn platform_resource_snapshot() -> ResourceSnapshot {
    use std::mem::MaybeUninit;

    fn usage(who: libc::c_int) -> libc::rusage {
        let mut usage = MaybeUninit::<libc::rusage>::uninit();
        // SAFETY: getrusage initializes the pointed-to rusage on a successful return.
        assert_eq!(unsafe { libc::getrusage(who, usage.as_mut_ptr()) }, 0);
        // SAFETY: the successful getrusage call above initialized the value.
        unsafe { usage.assume_init() }
    }

    let own = usage(libc::RUSAGE_SELF);
    let children = usage(libc::RUSAGE_CHILDREN);
    let status = fs::read_to_string("/proc/self/status").expect("process status metrics");
    let io = fs::read_to_string("/proc/self/io").expect("process I/O metrics");
    ResourceSnapshot {
        context_switches: Some(
            nonnegative(own.ru_nvcsw)
                .saturating_add(nonnegative(own.ru_nivcsw))
                .saturating_add(nonnegative(children.ru_nvcsw))
                .saturating_add(nonnegative(children.ru_nivcsw)),
        ),
        cpu_micros: Some(
            timeval_micros(own.ru_utime)
                .saturating_add(timeval_micros(own.ru_stime))
                .saturating_add(timeval_micros(children.ru_utime))
                .saturating_add(timeval_micros(children.ru_stime)),
        ),
        handles_or_fds: Some(
            u64::try_from(
                fs::read_dir("/proc/self/fd")
                    .expect("process file descriptor metrics")
                    .count(),
            )
            .expect("file descriptor count"),
        ),
        io_bytes: Some(
            proc_value(&io, "read_bytes:").saturating_add(proc_value(&io, "write_bytes:")),
        ),
        io_operations: Some(proc_value(&io, "syscr:").saturating_add(proc_value(&io, "syscw:"))),
        peak_rss_bytes: Some(
            nonnegative(own.ru_maxrss)
                .max(nonnegative(children.ru_maxrss))
                .saturating_mul(1_024),
        ),
        private_bytes: None,
        rss_bytes: Some(proc_value(&status, "VmRSS:").saturating_mul(1_024)),
        threads: Some(proc_value(&status, "Threads:")),
    }
}

#[cfg(target_os = "linux")]
fn proc_value(input: &str, name: &str) -> u64 {
    input
        .lines()
        .find_map(|line| line.strip_prefix(name))
        .and_then(|value| value.split_ascii_whitespace().next())
        .and_then(|value| value.parse().ok())
        .unwrap_or_default()
}

#[cfg(target_os = "linux")]
fn timeval_micros(value: libc::timeval) -> u64 {
    u64::try_from(value.tv_sec)
        .unwrap_or_default()
        .saturating_mul(1_000_000)
        .saturating_add(u64::try_from(value.tv_usec).unwrap_or_default())
}

#[cfg(target_os = "linux")]
fn nonnegative(value: libc::c_long) -> u64 {
    u64::try_from(value).unwrap_or_default()
}

#[cfg(windows)]
struct WindowsSnapshot(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl Drop for WindowsSnapshot {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;

        // SAFETY: The handle is owned by this guard and remains valid until this drop.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

#[cfg(windows)]
fn windows_thread_count(process_id: u32) -> u64 {
    use std::mem::size_of;
    use windows_sys::Win32::{
        Foundation::INVALID_HANDLE_VALUE,
        System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
        },
    };

    // SAFETY: The flags satisfy the documented system-wide thread snapshot contract.
    let handle = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    assert_ne!(handle, INVALID_HANDLE_VALUE, "thread snapshot failed");
    let snapshot = WindowsSnapshot(handle);
    let mut entry = THREADENTRY32 {
        dwSize: u32::try_from(size_of::<THREADENTRY32>()).expect("thread entry size"),
        ..THREADENTRY32::default()
    };
    // SAFETY: The snapshot is live and entry points to a correctly sized writable structure.
    assert_ne!(unsafe { Thread32First(snapshot.0, &raw mut entry) }, 0);
    let mut count = 0_u64;
    loop {
        if entry.th32OwnerProcessID == process_id {
            count = count.saturating_add(1);
        }
        // SAFETY: The snapshot and writable entry remain live for enumeration.
        if unsafe { Thread32Next(snapshot.0, &raw mut entry) } == 0 {
            break;
        }
    }
    count
}

#[cfg(windows)]
fn filetime_ticks(value: windows_sys::Win32::Foundation::FILETIME) -> u64 {
    (u64::from(value.dwHighDateTime) << 32) | u64::from(value.dwLowDateTime)
}

#[cfg(windows)]
fn platform_resource_snapshot() -> ResourceSnapshot {
    use std::mem::size_of;
    use windows_sys::Win32::{
        Foundation::FILETIME,
        System::{
            ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX},
            Threading::{
                GetCurrentProcess, GetCurrentProcessId, GetProcessHandleCount,
                GetProcessIoCounters, GetProcessTimes, IO_COUNTERS,
            },
        },
    };

    // SAFETY: GetCurrentProcess returns a pseudo-handle valid for the current process lifetime.
    let process = unsafe { GetCurrentProcess() };
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: All output pointers refer to live writable FILETIME values.
    assert_ne!(
        unsafe {
            GetProcessTimes(
                process,
                &raw mut creation,
                &raw mut exit,
                &raw mut kernel,
                &raw mut user,
            )
        },
        0,
        "process time metrics failed"
    );
    let mut io = IO_COUNTERS::default();
    // SAFETY: The process pseudo-handle is valid and io is a writable IO_COUNTERS value.
    assert_ne!(unsafe { GetProcessIoCounters(process, &raw mut io) }, 0);
    let mut memory = PROCESS_MEMORY_COUNTERS_EX {
        cb: u32::try_from(size_of::<PROCESS_MEMORY_COUNTERS_EX>()).expect("memory counter size"),
        ..PROCESS_MEMORY_COUNTERS_EX::default()
    };
    // SAFETY: The output pointer and byte size describe the live memory counter value.
    assert_ne!(
        unsafe {
            K32GetProcessMemoryInfo(
                process,
                (&raw mut memory).cast(),
                u32::try_from(size_of::<PROCESS_MEMORY_COUNTERS_EX>())
                    .expect("memory counter size"),
            )
        },
        0,
        "process memory metrics failed"
    );
    let threads = windows_thread_count(unsafe { GetCurrentProcessId() });
    let mut handles = 0_u32;
    // SAFETY: The process pseudo-handle is valid and handles is writable.
    assert_ne!(
        unsafe { GetProcessHandleCount(process, &raw mut handles) },
        0
    );

    ResourceSnapshot {
        context_switches: None,
        cpu_micros: Some(
            filetime_ticks(kernel)
                .saturating_add(filetime_ticks(user))
                .saturating_div(10),
        ),
        handles_or_fds: Some(u64::from(handles)),
        io_bytes: Some(
            io.ReadTransferCount
                .saturating_add(io.WriteTransferCount)
                .saturating_add(io.OtherTransferCount),
        ),
        io_operations: Some(
            io.ReadOperationCount
                .saturating_add(io.WriteOperationCount)
                .saturating_add(io.OtherOperationCount),
        ),
        peak_rss_bytes: Some(
            u64::try_from(memory.PeakWorkingSetSize).expect("peak working set fits u64"),
        ),
        private_bytes: Some(u64::try_from(memory.PrivateUsage).expect("private bytes fit u64")),
        rss_bytes: Some(u64::try_from(memory.WorkingSetSize).expect("working set fits u64")),
        threads: Some(threads),
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
fn platform_resource_snapshot() -> ResourceSnapshot {
    ResourceSnapshot::default()
}

fn compare_ripgrep(corpus: &Corpus, glob_output: &str, grep_output: &str) {
    let version = Command::new("rg")
        .arg("--version")
        .output()
        .expect("ripgrep is required for the manual performance gate");
    assert!(version.status.success(), "ripgrep --version failed");

    let rg_files = measure("rg-files", corpus.files, 0, || {
        run_rg(
            corpus.path(),
            &[
                "--files",
                "--hidden",
                "--glob",
                "!.git/**",
                "--glob",
                "**/*.rs",
                "--sort",
                "path",
                "--no-messages",
            ],
        )
    });
    assert_equivalent_prefix(glob_output, &rg_files, corpus.path(), "glob");

    let rg_grep = measure("rg-grep", corpus.files, corpus.searchable_bytes, || {
        run_rg(
            corpus.path(),
            &[
                "--count-matches",
                "--fixed-strings",
                "--smart-case",
                "--hidden",
                "--glob",
                "!.git/**",
                "--glob",
                "**/*.rs",
                "--sort",
                "path",
                "--no-messages",
                "needle",
            ],
        )
    });
    assert_equivalent_prefix(grep_output, &rg_grep, corpus.path(), "grep");
}

fn run_rg(root: &Path, args: &[&str]) -> String {
    let output = Command::new("rg")
        .args(args)
        .current_dir(root)
        .output()
        .expect("run ripgrep comparison");
    assert!(output.status.success(), "ripgrep comparison failed");
    String::from_utf8(output.stdout).expect("ripgrep comparison output is UTF-8")
}

fn assert_equivalent_prefix(model: &str, rg: &str, root: &Path, operation: &str) {
    let root = root.to_string_lossy();
    let model_entries = model
        .lines()
        .filter_map(|line| line.strip_prefix(root.as_ref()))
        .map(normalize_entry)
        .collect::<Vec<_>>();
    let rg_entries = rg.lines().map(normalize_entry).collect::<Vec<_>>();
    assert!(!model_entries.is_empty(), "{operation} comparison is empty");
    assert!(
        rg_entries.len() >= model_entries.len(),
        "ripgrep returned fewer {operation} entries"
    );
    assert_eq!(
        model_entries,
        rg_entries[..model_entries.len()],
        "{operation} and ripgrep differ under equivalent semantics"
    );
}

fn normalize_entry(entry: &str) -> String {
    entry
        .trim_start_matches(['/', '\\'])
        .strip_prefix("./")
        .unwrap_or(entry.trim_start_matches(['/', '\\']))
        .replace('\\', "/")
}

#[test]
#[ignore = "manual performance corpus"]
fn scale_001_000_files() {
    benchmark(1_000);
}

#[test]
#[ignore = "manual performance corpus"]
fn scale_100_000_files() {
    benchmark(100_000);
}

#[test]
#[ignore = "manual performance corpus"]
fn scale_1_000_000_files() {
    benchmark(1_000_000);
}
