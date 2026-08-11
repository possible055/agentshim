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

#[path = "stdio_performance/harness.rs"]
mod harness;

use harness::{ResourceMonitor, Session};

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

fn percentile(samples: &[f64], numerator: usize, denominator: usize) -> f64 {
    let index = samples
        .len()
        .saturating_mul(numerator)
        .div_ceil(denominator)
        .saturating_sub(1)
        .min(samples.len() - 1);
    samples[index]
}
