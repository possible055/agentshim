use std::{ffi::OsString, fs, path::Path, sync::Arc, time::Instant};

use codexshim::bench_support::{
    FileAccess, ReadScope, RepositoryRoot,
    glob::{self, GlobRequest, GlobTraversal},
    grep::{self, GrepMode, GrepRequest},
    read::{self, ReadRequest},
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

const WARM_SAMPLES: usize = 7;
const CONCURRENCY_LEVELS: [usize; 3] = [1, 4, 8];
const GREP_LANES: [usize; 5] = [1, 2, 4, 8, 16];
const BENCH_SCALES_ENV: &str = "CODEXSHIM_BENCH_SCALES";
const BENCH_QUICK_ENV: &str = "CODEXSHIM_BENCH_QUICK";
const GLOB_VARIANTS_ONLY_ENV: &str = "CODEXSHIM_BENCH_GLOB_VARIANTS_ONLY";
const GLOB_P95_ENV: &str = "CODEXSHIM_BENCH_MAX_GLOB_P95_MS_PER_1K";
const GREP_P95_ENV: &str = "CODEXSHIM_BENCH_MAX_GREP_P95_MS_PER_1K";
const READ_P95_ENV: &str = "CODEXSHIM_BENCH_MAX_READ_P95_MS";

fn main() {
    for files in fixture_scales() {
        benchmark_scale(files);
        println!(
            "{}",
            json!({ "benchmark": "scale_complete", "fixture_files": files })
        );
    }
    std::process::exit(0);
}

fn fixture_scales() -> Vec<usize> {
    let configured = std::env::var(BENCH_SCALES_ENV).unwrap_or_else(|_| "1000".to_owned());
    let scales = configured
        .split(',')
        .map(str::trim)
        .map(|value| value.parse::<usize>().expect("fixture scale is an integer"))
        .collect::<Vec<_>>();
    assert!(
        scales
            .iter()
            .all(|scale| matches!(*scale, 1_000 | 10_000 | 100_000 | 1_000_000)),
        "{BENCH_SCALES_ENV} accepts only 1000,10000,100000,1000000"
    );
    scales
}

fn benchmark_scale(files: usize) {
    let fixture = tempfile::tempdir().expect("performance fixture");
    let repository_directory = fixture.path().join("repository");
    let repository_corpus = repository_directory.join("corpus");
    let codex_home = fixture.path().join("codex");
    let codex_corpus = codex_home.join("skills").join("corpus");
    create_corpus(&repository_corpus, files);
    create_corpus(&codex_corpus, files);

    let root = Arc::new(RepositoryRoot::open(&repository_directory).expect("repository root"));
    let previous_codex_home = std::env::var_os("CODEX_HOME");
    set_codex_home(Some(codex_home.into_os_string()));
    let normal_access = Arc::new(FileAccess::new(Arc::clone(&root), ReadScope::Normal));
    set_codex_home(previous_codex_home);
    let unrestricted_access = Arc::new(FileAccess::new(Arc::clone(&root), ReadScope::Unrestricted));
    let repository_access = Arc::new(FileAccess::new(Arc::clone(&root), ReadScope::Normal));

    benchmark_tools(
        "repository",
        files,
        &repository_access,
        "corpus",
        "corpus/shard-000000/file-000000000.rs",
    );
    let codex_path = codex_corpus.to_string_lossy().into_owned();
    let codex_file = codex_corpus
        .join("shard-000000/file-000000000.rs")
        .to_string_lossy()
        .into_owned();
    benchmark_tools(
        "normal_codex",
        files,
        &normal_access,
        &codex_path,
        &codex_file,
    );
    benchmark_tools(
        "unrestricted",
        files,
        &unrestricted_access,
        &codex_path,
        &codex_file,
    );
}

fn set_codex_home(value: Option<OsString>) {
    match value {
        Some(value) => unsafe { std::env::set_var("CODEX_HOME", value) },
        None => unsafe { std::env::remove_var("CODEX_HOME") },
    }
}

fn create_corpus(directory: &Path, files: usize) {
    for index in 0..files {
        let shard = directory.join(format!("shard-{:06}", index / 1_000));
        if index % 1_000 == 0 {
            fs::create_dir_all(&shard).expect("corpus shard");
        }
        fs::write(
            shard.join(format!("file-{index:09}.rs")),
            format!("pub fn fixture_{index}() {{}}\nneedle-{index}\n"),
        )
        .expect("corpus file");
    }
}

fn benchmark_tools(
    scope: &str,
    files: usize,
    access: &Arc<FileAccess>,
    directory: &str,
    read_path: &str,
) {
    let cancellation = CancellationToken::new();
    for (operation, traversal) in [
        ("glob_serial", GlobTraversal::Serial),
        ("glob_parallel_256", GlobTraversal::ParallelBatched),
        ("glob_adaptive", GlobTraversal::Adaptive),
    ] {
        measure(scope, operation, files, || {
            glob::execute_with_traversal(
                access,
                &GlobRequest {
                    pattern: "**/*.rs".to_owned(),
                    path: Some(directory.to_owned()),
                    include_ignored: None,
                    offset: None,
                    limit: Some(1_000),
                },
                &cancellation,
                traversal,
            )
            .expect("glob benchmark")
        });
    }
    if std::env::var_os(GLOB_VARIANTS_ONLY_ENV).is_some_and(|value| value == "1") {
        return;
    }
    for &lanes in grep_lanes() {
        measure(scope, &format!("grep_lanes_{lanes}"), files, || {
            grep::execute(
                access,
                &GrepRequest {
                    pattern: "needle-".to_owned(),
                    path: Some(directory.to_owned()),
                    glob: Some("**/*.rs".to_owned()),
                    mode: Some(GrepMode::Count),
                    fixed_strings: Some(true),
                    case: None,
                    context_lines: None,
                    offset: None,
                    limit: Some(1_000),
                },
                lanes,
                &cancellation,
            )
            .expect("grep benchmark")
        });
    }
    measure(scope, "read", files, || {
        read::execute(
            access,
            &ReadRequest {
                path: read_path.to_owned(),
                start_line: Some(1),
                line_count: Some(2_000),
                encoding: None,
            },
            &cancellation,
        )
        .expect("read benchmark")
    });
}

fn measure(scope: &str, operation: &str, files: usize, execute: impl Fn() -> String + Sync) {
    measure_with(scope, operation, files, execute, |left, right| {
        left == right
    });
}

fn measure_with(
    scope: &str,
    operation: &str,
    files: usize,
    execute: impl Fn() -> String + Sync,
    equivalent: impl Fn(&str, &str) -> bool + Sync,
) {
    let cold_started = Instant::now();
    let expected = execute();
    let cold_ms = cold_started.elapsed().as_secs_f64() * 1_000.0;
    assert!(!expected.is_empty(), "{operation} returned no output");

    let warm_samples = if quick_mode() { 1 } else { WARM_SAMPLES };
    let mut warm_ms = Vec::with_capacity(warm_samples);
    for _ in 0..warm_samples {
        let started = Instant::now();
        assert!(
            equivalent(&execute(), &expected),
            "{operation} output changed"
        );
        warm_ms.push(started.elapsed().as_secs_f64() * 1_000.0);
    }
    warm_ms.sort_by(f64::total_cmp);
    let p95_ms = percentile(&warm_ms, 95, 100);
    let p95_limit_ms = p95_limit(operation, files);
    assert!(
        p95_ms <= p95_limit_ms,
        "{scope} {operation} p95 {p95_ms:.3} ms exceeds {p95_limit_ms:.3} ms"
    );

    let concurrent = concurrency_levels()
        .iter()
        .copied()
        .map(|calls| {
            let started = Instant::now();
            std::thread::scope(|scope| {
                let workers = (0..calls)
                    .map(|_| scope.spawn(&execute))
                    .collect::<Vec<_>>();
                for worker in workers {
                    assert!(
                        equivalent(&worker.join().expect("benchmark worker"), &expected),
                        "{operation} concurrent output changed"
                    );
                }
            });
            let elapsed = started.elapsed().as_secs_f64();
            json!({
                "calls": calls,
                "elapsed_ms": elapsed * 1_000.0,
                "throughput_per_second": f64::from(u32::try_from(calls).expect("bounded concurrency")) / elapsed,
            })
        })
        .collect::<Vec<_>>();

    println!(
        "{}",
        json!({
            "scope": scope,
            "operation": operation,
            "fixture_files": files,
            "cold_ms": cold_ms,
            "warm_ms": warm_ms,
            "p50_ms": percentile(&warm_ms, 50, 100),
            "p95_ms": p95_ms,
            "p95_limit_ms": p95_limit_ms,
            "p99_ms": percentile(&warm_ms, 99, 100),
            "concurrent": concurrent,
            "output_bytes": expected.len(),
            "output_equivalent": true,
        })
    );
}

fn p95_limit(operation: &str, files: usize) -> f64 {
    if operation == "read" {
        return configured_limit(READ_P95_ENV, 10.0);
    }
    let scale = files.div_ceil(1_000) as f64;
    if operation.starts_with("glob") {
        return configured_limit(GLOB_P95_ENV, 250.0) * scale;
    }
    if operation.starts_with("grep_lanes_") {
        return configured_limit(GREP_P95_ENV, 2_000.0) * scale;
    }
    panic!("missing p95 limit for {operation}");
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

fn quick_mode() -> bool {
    std::env::var_os(BENCH_QUICK_ENV).is_some_and(|value| value == "1")
}

fn grep_lanes() -> &'static [usize] {
    if quick_mode() {
        &GREP_LANES[4..]
    } else {
        &GREP_LANES
    }
}

fn concurrency_levels() -> &'static [usize] {
    if quick_mode() {
        &CONCURRENCY_LEVELS[..1]
    } else {
        &CONCURRENCY_LEVELS
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
