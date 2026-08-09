use std::{
    ffi::OsString,
    fmt::Write as _,
    fs,
    path::Path,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};

use codexshim::bench_support::{
    FileAccess, OpenReadStrategy, ReadScope, RepositoryRoot, ResolvedPath,
    glob::{self, GlobRequest, GlobTraversal},
    grep::{self, GrepMode, GrepRequest},
    open_read_batches,
    read::{self, ReadRequest},
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

const WARM_SAMPLES: usize = 7;
const CONCURRENCY_LEVELS: [usize; 3] = [1, 8, 16];
const GREP_LANES: [usize; 5] = [1, 2, 4, 8, 16];
const BENCH_SCALES_ENV: &str = "CODEXSHIM_BENCH_SCALES";
const BENCH_SCOPES_ENV: &str = "CODEXSHIM_BENCH_SCOPES";
const BENCH_QUICK_ENV: &str = "CODEXSHIM_BENCH_QUICK";
const BENCH_QUICK_CONCURRENT_ENV: &str = "CODEXSHIM_BENCH_QUICK_CONCURRENT";
const OPEN_BATCH_ONLY_ENV: &str = "CODEXSHIM_BENCH_OPEN_BATCH_ONLY";
const GREP_LANES_ENV: &str = "CODEXSHIM_BENCH_GREP_LANES";
const GREP_TRAVERSALS_ENV: &str = "CODEXSHIM_BENCH_GREP_TRAVERSALS";
const GREP_WORKLOAD_ENV: &str = "CODEXSHIM_BENCH_GREP_WORKLOAD";
const GREP_GLOB_ENV: &str = "CODEXSHIM_BENCH_GREP_GLOB";
const GREP_SELECTED_FILES_ENV: &str = "CODEXSHIM_BENCH_GREP_SELECTED_FILES";
const GLOB_VARIANTS_ONLY_ENV: &str = "CODEXSHIM_BENCH_GLOB_VARIANTS_ONLY";
const GLOB_PROFILE_ONLY_ENV: &str = "CODEXSHIM_BENCH_GLOB_PROFILE_ONLY";
const GLOB_PATTERN_ENV: &str = "CODEXSHIM_BENCH_GLOB_PATTERN";
const GLOB_TRAVERSALS_ENV: &str = "CODEXSHIM_BENCH_GLOB_TRAVERSALS";
const GREP_VARIANTS_ONLY_ENV: &str = "CODEXSHIM_BENCH_GREP_VARIANTS_ONLY";
const GREP_PROFILE_ENV: &str = "CODEXSHIM_BENCH_PROFILE_GREP";
const GREP_PROFILE_ONLY_ENV: &str = "CODEXSHIM_BENCH_GREP_PROFILE_ONLY";
const GREP_COMPACT_PROFILE_ENV: &str = "CODEXSHIM_BENCH_GREP_COMPACT_PROFILE";
const GREP_SOURCES_ENV: &str = "CODEXSHIM_BENCH_GREP_SOURCES";
const GREP_PATHNAME_REOPEN_ENV: &str = "CODEXSHIM_BENCH_GREP_PATHNAME_REOPEN";
const GREP_MODE_ENV: &str = "CODEXSHIM_BENCH_GREP_MODE";
const BENCH_COMMIT_ENV: &str = "CODEXSHIM_BENCH_COMMIT";
const BENCH_WORKTREE_ENV: &str = "CODEXSHIM_BENCH_WORKTREE";
const GLOB_P95_ENV: &str = "CODEXSHIM_BENCH_MAX_GLOB_P95_MS_PER_1K";
const GREP_P95_ENV: &str = "CODEXSHIM_BENCH_MAX_GREP_P95_MS_PER_1K";
const READ_P95_ENV: &str = "CODEXSHIM_BENCH_MAX_READ_P95_MS";

#[derive(Clone, Copy)]
enum GrepFileSize {
    Legacy,
    OneKiB,
    SixtyFourKiB,
    FourMiB,
}

#[derive(Clone, Copy)]
enum MatchDensity {
    Legacy,
    None,
    Rare,
    Dense,
}

#[derive(Clone, Copy)]
struct GrepWorkload {
    name: &'static str,
    file_size: GrepFileSize,
    density: MatchDensity,
}

fn main() {
    init_mmap_trace();
    for files in fixture_scales() {
        benchmark_scale(files);
        println!(
            "{}",
            json!({ "benchmark": "scale_complete", "fixture_files": files })
        );
    }
    std::process::exit(0);
}

struct MmapTraceLogger;

static MMAP_TRACE_LOGGER: MmapTraceLogger = MmapTraceLogger;
static MMAP_SELECTED: AtomicUsize = AtomicUsize::new(0);
static MMAP_FALLBACK: AtomicUsize = AtomicUsize::new(0);

impl log::Log for MmapTraceLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Trace
            && metadata.target().starts_with("grep_searcher::searcher")
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let message = record.args().to_string();
        if message.contains("searching via memory map") {
            MMAP_SELECTED.fetch_add(1, Ordering::Relaxed);
        } else if message.contains("searching using generic reader") {
            MMAP_FALLBACK.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn flush(&self) {}
}

fn init_mmap_trace() {
    log::set_logger(&MMAP_TRACE_LOGGER).expect("benchmark logger");
    log::set_max_level(log::LevelFilter::Trace);
}

fn reset_mmap_trace() {
    MMAP_SELECTED.store(0, Ordering::Relaxed);
    MMAP_FALLBACK.store(0, Ordering::Relaxed);
}

fn mmap_trace() -> (usize, usize) {
    (
        MMAP_SELECTED.load(Ordering::Relaxed),
        MMAP_FALLBACK.load(Ordering::Relaxed),
    )
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
    let codex_corpus = fs::canonicalize(codex_corpus).expect("canonical Codex corpus");

    let root = Arc::new(RepositoryRoot::open(&repository_directory).expect("repository root"));
    let previous_codex_home = std::env::var_os("CODEX_HOME");
    set_codex_home(Some(codex_home.into_os_string()));
    let normal_access = Arc::new(FileAccess::new(Arc::clone(&root), ReadScope::Normal));
    set_codex_home(previous_codex_home);
    let unrestricted_access = Arc::new(FileAccess::new(Arc::clone(&root), ReadScope::Unrestricted));
    let repository_access = Arc::new(FileAccess::new(Arc::clone(&root), ReadScope::Normal));

    if scope_enabled("repository") {
        benchmark_tools(
            "repository",
            files,
            &repository_access,
            "corpus",
            "corpus/shard-000000/file-000000000.rs",
        );
    }
    let codex_path = codex_corpus.to_string_lossy().into_owned();
    let codex_file = codex_corpus
        .join("shard-000000/file-000000000.rs")
        .to_string_lossy()
        .into_owned();
    if scope_enabled("normal_codex") {
        benchmark_tools(
            "normal_codex",
            files,
            &normal_access,
            &codex_path,
            &codex_file,
        );
    }
    if scope_enabled("unrestricted") {
        benchmark_tools(
            "unrestricted",
            files,
            &unrestricted_access,
            &codex_path,
            &codex_file,
        );
    }
}

fn scope_enabled(scope: &str) -> bool {
    std::env::var(BENCH_SCOPES_ENV).map_or(true, |value| {
        value.split(',').map(str::trim).any(|value| value == scope)
    })
}

fn set_codex_home(value: Option<OsString>) {
    match value {
        Some(value) => unsafe { std::env::set_var("CODEX_HOME", value) },
        None => unsafe { std::env::remove_var("CODEX_HOME") },
    }
}

fn create_corpus(directory: &Path, files: usize) {
    let workload = grep_workload();
    let selected_files = workload.selected_files(files);
    for index in 0..files {
        let shard = directory.join(format!("shard-{:06}", index / 1_000));
        if index % 1_000 == 0 {
            fs::create_dir_all(&shard).expect("corpus shard");
        }
        let selected = index < selected_files;
        let content = if selected {
            workload.content(index)
        } else {
            format!("pub fn fixture_{index}() {{}}\n")
        };
        let suffix = if selected && !matches!(workload.file_size, GrepFileSize::Legacy) {
            "selected.rs"
        } else {
            "rs"
        };
        fs::write(shard.join(format!("file-{index:09}.{suffix}")), content).expect("corpus file");
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
    if std::env::var_os(OPEN_BATCH_ONLY_ENV).is_some_and(|value| value == "1") {
        benchmark_open_batches(scope, files, access, directory);
        return;
    }
    if std::env::var_os(GREP_PROFILE_ONLY_ENV).is_some_and(|value| value == "1") {
        profile_grep(scope, files, access, directory, &cancellation);
        return;
    }
    if std::env::var_os(GLOB_PROFILE_ONLY_ENV).is_some_and(|value| value == "1") {
        profile_glob(scope, files, access, directory, &cancellation);
        return;
    }
    let grep_variants_only =
        std::env::var_os(GREP_VARIANTS_ONLY_ENV).is_some_and(|value| value == "1");
    if !grep_variants_only {
        for (operation, traversal) in [
            ("glob_serial", GlobTraversal::Serial),
            ("glob_parallel_256", GlobTraversal::ParallelBatched),
            ("glob_adaptive", GlobTraversal::Adaptive),
        ] {
            measure(scope, operation, files, || {
                glob::execute_with_traversal(
                    access,
                    &glob_request(directory),
                    &cancellation,
                    traversal,
                )
                .expect("glob benchmark")
            });
        }
    }
    if std::env::var_os(GLOB_VARIANTS_ONLY_ENV).is_some_and(|value| value == "1") {
        return;
    }
    for &lanes in grep_lanes() {
        if grep_variants_only {
            for &(source_name, source) in grep_sources() {
                for &(reopen_name, pathname_reopen) in pathname_reopen_variants() {
                    let operation =
                        format!("grep_{source_name}_reopen_{reopen_name}_lanes_{lanes}");
                    measure(scope, &operation, files, || {
                        grep::execute_with_variant(
                            access,
                            &grep_request(directory, files),
                            lanes,
                            &cancellation,
                            grep::GrepTraversal::Adaptive,
                            grep::GrepBenchmarkVariant {
                                source,
                                pathname_reopen,
                            },
                        )
                        .expect("grep variant benchmark")
                    });
                }
            }
        } else {
            measure(scope, &format!("grep_lanes_{lanes}"), files, || {
                grep::execute(
                    access,
                    &grep_request(directory, files),
                    lanes,
                    &cancellation,
                )
                .expect("grep benchmark")
            });
        }
    }
    if std::env::var_os(GREP_PROFILE_ENV).is_some_and(|value| value == "1") {
        profile_grep(scope, files, access, directory, &cancellation);
    }
    if grep_variants_only {
        return;
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

fn benchmark_open_batches(scope: &str, files: usize, access: &FileAccess, directory: &str) {
    let path_count = files.min(1_000);
    let paths = (0..path_count)
        .map(|index| {
            Path::new(directory)
                .join("shard-000000")
                .join(format!("file-{index:09}.rs"))
        })
        .map(|path| access.resolve(&path).expect("open batch path"))
        .collect::<Vec<_>>();
    for batch_size in [1, 4, 8, 16] {
        for (strategy_name, strategy) in [
            ("individual", OpenReadStrategy::Individual),
            ("same_parent", OpenReadStrategy::SameParentBatch),
        ] {
            measure_open_batches(
                scope,
                files,
                &paths,
                access,
                batch_size,
                strategy_name,
                strategy,
            );
        }
    }
}

fn measure_open_batches(
    scope: &str,
    files: usize,
    paths: &[ResolvedPath],
    access: &FileAccess,
    batch_size: usize,
    strategy_name: &str,
    strategy: OpenReadStrategy,
) {
    let cold_started = Instant::now();
    let opened = open_read_batches(access, paths, batch_size, strategy).expect("cold batch open");
    let cold_ms = cold_started.elapsed().as_secs_f64() * 1_000.0;
    assert_eq!(opened, paths.len());

    let warm_samples = if quick_mode() { 1 } else { WARM_SAMPLES };
    let mut warm_ms = Vec::with_capacity(warm_samples);
    for _ in 0..warm_samples {
        let started = Instant::now();
        let opened =
            open_read_batches(access, paths, batch_size, strategy).expect("warm batch open");
        warm_ms.push(started.elapsed().as_secs_f64() * 1_000.0);
        assert_eq!(opened, paths.len());
    }
    warm_ms.sort_by(f64::total_cmp);
    println!(
        "{}",
        json!({
            "benchmark": "open_read_batches",
            "scope": scope,
            "fixture_files": files,
            "opened_files": paths.len(),
            "batch_size": batch_size,
            "strategy": strategy_name,
            "cold_ms": cold_ms,
            "warm_ms": warm_ms,
            "p50_ms": percentile(&warm_ms, 50, 100),
            "p95_ms": percentile(&warm_ms, 95, 100),
        })
    );
}

fn glob_request(directory: &str) -> GlobRequest {
    GlobRequest {
        pattern: std::env::var(GLOB_PATTERN_ENV).unwrap_or_else(|_| "**/*.rs".to_owned()),
        path: Some(directory.to_owned()),
        include_ignored: None,
        offset: None,
        limit: Some(1_000),
    }
}

fn profile_glob(
    scope: &str,
    files: usize,
    access: &Arc<FileAccess>,
    directory: &str,
    cancellation: &CancellationToken,
) {
    let request = glob_request(directory);
    let mut expected = None;
    for &(traversal_name, traversal) in glob_traversals() {
        let profile =
            glob::execute_profiled_with_traversal(access, &request, cancellation, traversal)
                .expect("profiled glob");
        let expected = expected.get_or_insert_with(|| profile.output.clone());
        assert_eq!(
            profile.output.as_str(),
            expected.as_str(),
            "profiled glob output changed"
        );
        let stages = profile.timings;
        let sequential_ns = stages
            .setup_ns
            .saturating_add(stages.traversal_wall_ns)
            .saturating_add(stages.final_sort_ns)
            .saturating_add(stages.render_ns);
        assert!(
            stages.total_ns >= sequential_ns,
            "glob stage wall time exceeds total"
        );
        println!(
            "{}",
            json!({
                "benchmark": "glob_profile",
                "scope": scope,
                "fixture_files": files,
                "traversal": traversal_name,
                "wall_ms": {
                    "total": nanos_to_ms(stages.total_ns),
                    "setup": nanos_to_ms(stages.setup_ns),
                    "traversal": nanos_to_ms(stages.traversal_wall_ns),
                    "final_sort": nanos_to_ms(stages.final_sort_ns),
                    "render": nanos_to_ms(stages.render_ns),
                },
                "worker_cpu_ms": {
                    "merge_wait": nanos_to_ms(stages.merge_wait_worker_ns),
                    "merge_work": nanos_to_ms(stages.merge_work_worker_ns),
                },
                "batches": stages.batches,
                "matched_entries": stages.matched_entries,
                "retained_entries": stages.retained_entries,
                "retained_memory_bytes": stages.retained_memory_bytes,
                "output_bytes": profile.output.len(),
                "output_equivalent": true,
            })
        );
    }
}

fn grep_request(directory: &str, files: usize) -> GrepRequest {
    let workload = grep_workload();
    let _selected_files = workload.selected_files(files);
    GrepRequest {
        pattern: "needle-".to_owned(),
        path: Some(directory.to_owned()),
        glob: Some(std::env::var(GREP_GLOB_ENV).unwrap_or_else(|_| workload.glob().to_owned())),
        mode: Some(grep_mode()),
        fixed_strings: Some(true),
        case: None,
        context_lines: None,
        offset: None,
        limit: Some(1_000),
    }
}

fn glob_traversals() -> &'static [(&'static str, glob::GlobTraversal)] {
    const DEFAULT: [(&str, glob::GlobTraversal); 3] = [
        ("serial", glob::GlobTraversal::Serial),
        ("parallel_256", glob::GlobTraversal::ParallelBatched),
        ("adaptive", glob::GlobTraversal::Adaptive),
    ];
    static CONFIGURED: OnceLock<Option<Vec<(&'static str, glob::GlobTraversal)>>> = OnceLock::new();
    CONFIGURED
        .get_or_init(|| {
            std::env::var(GLOB_TRAVERSALS_ENV).ok().map(|value| {
                let traversals = value
                    .split(',')
                    .map(str::trim)
                    .map(|value| match value {
                        "serial" => ("serial", glob::GlobTraversal::Serial),
                        "parallel_256" => ("parallel_256", glob::GlobTraversal::ParallelBatched),
                        "adaptive" => ("adaptive", glob::GlobTraversal::Adaptive),
                        "serial_prefix" => {
                            ("serial_prefix", glob::GlobTraversal::SerialLiteralPrefix)
                        }
                        "parallel_256_prefix" => (
                            "parallel_256_prefix",
                            glob::GlobTraversal::ParallelBatchedLiteralPrefix,
                        ),
                        _ => panic!(
                            "{GLOB_TRAVERSALS_ENV} accepts only serial,parallel_256,adaptive,\
                             serial_prefix,parallel_256_prefix"
                        ),
                    })
                    .collect::<Vec<_>>();
                assert!(!traversals.is_empty(), "{GLOB_TRAVERSALS_ENV} is empty");
                traversals
            })
        })
        .as_deref()
        .unwrap_or(&DEFAULT)
}

fn profile_grep(
    scope: &str,
    files: usize,
    access: &Arc<FileAccess>,
    directory: &str,
    cancellation: &CancellationToken,
) {
    let request = grep_request(directory, files);
    for &lanes in grep_lanes() {
        let mut expected = None;
        for &(traversal_name, traversal) in grep_traversals() {
            for &(source_name, source) in grep_sources() {
                for &(reopen_name, pathname_reopen) in pathname_reopen_variants() {
                    read::reset_fingerprint_metrics();
                    reset_mmap_trace();
                    let profile = grep::execute_profiled_with_variant(
                        access,
                        &request,
                        lanes,
                        cancellation,
                        traversal,
                        grep::GrepBenchmarkVariant {
                            source,
                            pathname_reopen,
                        },
                    )
                    .expect("profiled grep");
                    let fingerprint = read::fingerprint_metrics();
                    let mmap = mmap_trace();
                    if expected.is_none() {
                        expected = Some(profile.output.clone());
                    }
                    emit_grep_profile(&GrepProfileEmission {
                        scope,
                        files,
                        traversal: traversal_name,
                        source: source_name,
                        pathname_reopen: reopen_name,
                        expected: expected.as_deref().expect("profile baseline"),
                        profile,
                        fingerprint,
                        mmap,
                    });
                }
            }
        }
    }
}

struct GrepProfileEmission<'a> {
    scope: &'a str,
    files: usize,
    traversal: &'a str,
    source: &'a str,
    pathname_reopen: &'a str,
    expected: &'a str,
    profile: grep::ProfiledGrep,
    fingerprint: read::FingerprintMetrics,
    mmap: (usize, usize),
}

fn emit_grep_profile(emission: &GrepProfileEmission<'_>) {
    assert_eq!(
        emission.profile.output, emission.expected,
        "profiled grep output changed"
    );
    let stages = &emission.profile.timings;
    let sequential_ns = stages
        .setup_ns
        .saturating_add(stages.candidate_traversal_ns)
        .saturating_add(stages.candidate_sort_ns)
        .saturating_add(stages.search_wall_ns)
        .saturating_add(stages.render_ns);
    assert!(
        stages.total_ns >= sequential_ns,
        "grep stage wall time exceeds total"
    );
    if std::env::var_os(GREP_COMPACT_PROFILE_ENV).is_some_and(|value| value == "1") {
        println!(
            "{}",
            json!({
                "benchmark": "grep_profile",
                "binary_commit": benchmark_identity().0,
                "binary_worktree": benchmark_identity().1,
                "scope": emission.scope,
                "workload": grep_workload().name,
                "fixture_files": emission.files,
                "selected_files": grep_workload().selected_files(emission.files),
                "traversal": emission.traversal,
                "source": emission.source,
                "pathname_reopen": emission.pathname_reopen,
                "open_strategy": "default",
                "sort": std::env::var("CODEXSHIM_BENCH_GREP_SORT")
                    .unwrap_or_else(|_| "heapsort".to_owned()),
                "lanes": stages.lanes,
                "candidate_count": stages.candidate_count,
                "searched_candidates": stages.searched_candidates,
                "matched_candidates": stages.matched_candidates,
                "candidate_retained_memory_bytes": stages.candidate_retained_memory_bytes,
                "candidate_vec_capacity": stages.candidate_vec_capacity,
                "candidate_soft_target_crossings": stages.candidate_soft_target_crossings,
                "candidate_path_bytes": {
                    "key": stages.candidate_key_bytes,
                    "capability_key": stages.candidate_capability_key_bytes,
                    "absolute": stages.candidate_absolute_bytes,
                    "sort_key": stages.candidate_sort_key_bytes,
                    "slash_path": stages.candidate_slash_path_bytes,
                },
                "candidate_path_capacity": {
                    "key": stages.candidate_key_capacity,
                    "capability_key": stages.candidate_capability_key_capacity,
                    "absolute": stages.candidate_absolute_capacity,
                    "sort_key": stages.candidate_sort_key_capacity,
                    "slash_path": stages.candidate_slash_path_capacity,
                },
                "candidate_policy": std::env::var("CODEXSHIM_BENCH_GREP_CANDIDATE_POLICY")
                    .unwrap_or_else(|_| "soft".to_owned()),
                "total_ms": nanos_to_ms(stages.total_ns),
                "traversal_ms": nanos_to_ms(stages.candidate_traversal_ns),
                "search_ms": nanos_to_ms(stages.search_wall_ns),
                "output_equivalent": true,
            })
        );
        return;
    }
    println!("{}", full_grep_profile_json(emission));
}

fn full_grep_profile_json(emission: &GrepProfileEmission<'_>) -> serde_json::Value {
    let stages = &emission.profile.timings;
    let fingerprint = emission.fingerprint;
    let mmap = emission.mmap;
    json!({
        "benchmark": "grep_profile",
        "binary_commit": benchmark_identity().0,
        "binary_worktree": benchmark_identity().1,
        "memory_policy": "soft_gate_128_mib",
        "candidate_policy": std::env::var("CODEXSHIM_BENCH_GREP_CANDIDATE_POLICY")
            .unwrap_or_else(|_| "soft".to_owned()),
        "process_rss_hard_limit_bytes": 1024_u64 * 1024 * 1024,
        "scope": emission.scope,
        "workload": grep_workload().name,
        "fixture_files": emission.files,
        "selected_files": grep_workload().selected_files(emission.files),
        "traversal": emission.traversal,
        "source": emission.source,
        "pathname_reopen": emission.pathname_reopen,
        "open_strategy": "default",
        "sort": std::env::var("CODEXSHIM_BENCH_GREP_SORT")
            .unwrap_or_else(|_| "heapsort".to_owned()),
        "lanes": stages.lanes,
        "candidate_count": stages.candidate_count,
        "searched_candidates": stages.searched_candidates,
        "matched_candidates": stages.matched_candidates,
        "candidate_retained_memory_bytes": stages.candidate_retained_memory_bytes,
        "candidate_vec_capacity": stages.candidate_vec_capacity,
        "candidate_soft_target_crossings": stages.candidate_soft_target_crossings,
        "candidate_path_bytes": {
            "key": stages.candidate_key_bytes,
            "capability_key": stages.candidate_capability_key_bytes,
            "absolute": stages.candidate_absolute_bytes,
            "sort_key": stages.candidate_sort_key_bytes,
            "slash_path": stages.candidate_slash_path_bytes,
        },
        "candidate_path_capacity": {
            "key": stages.candidate_key_capacity,
            "capability_key": stages.candidate_capability_key_capacity,
            "absolute": stages.candidate_absolute_capacity,
            "sort_key": stages.candidate_sort_key_capacity,
            "slash_path": stages.candidate_slash_path_capacity,
        },
        "wall_ms": {
            "total": nanos_to_ms(stages.total_ns),
            "setup": nanos_to_ms(stages.setup_ns),
            "candidate_traversal": nanos_to_ms(stages.candidate_traversal_ns),
            "candidate_sort": nanos_to_ms(stages.candidate_sort_ns),
            "search": nanos_to_ms(stages.search_wall_ns),
            "ordered_reduce": nanos_to_ms(stages.ordered_reduce_wall_ns),
            "render": nanos_to_ms(stages.render_ns),
        },
        "worker_cpu_ms": {
            "open_before_fingerprint": nanos_to_ms(stages.search_open_worker_ns),
            "open_handle": nanos_to_ms(stages.search_open_handle_worker_ns),
            "symlink_metadata": nanos_to_ms(stages.search_symlink_metadata_worker_ns),
            "scan": nanos_to_ms(stages.search_scan_worker_ns),
            "capture_read": nanos_to_ms(stages.capture_read_worker_ns),
            "classification": nanos_to_ms(stages.classification_worker_ns),
            "search_reader": nanos_to_ms(stages.search_reader_worker_ns),
            "search_file": nanos_to_ms(stages.search_file_worker_ns),
            "search_slice": nanos_to_ms(stages.search_slice_worker_ns),
            "before_fingerprint": nanos_to_ms(stages.search_before_fingerprint_worker_ns),
            "after_fingerprint": nanos_to_ms(stages.search_after_fingerprint_worker_ns),
            "pathname_reopen": nanos_to_ms(stages.search_pathname_reopen_worker_ns),
            "pathname_fingerprint": nanos_to_ms(stages.search_pathname_fingerprint_worker_ns),
            "after_identity_verification": nanos_to_ms(stages.search_verify_worker_ns),
            "ordered_wait": nanos_to_ms(stages.ordered_wait_worker_ns),
        },
        "source_counts": {
            "search_reader": stages.search_reader_files,
            "search_file": stages.search_file_files,
            "search_slice": stages.search_slice_files,
            "mmap_requested": stages.mmap_requested_files,
            "mmap_selected": mmap.0,
            "mmap_fallback": mmap.1,
            "pathname_reopens": stages.pathname_reopens,
        },
        "bytes": {
            "render_copy": stages.render_copy_bytes,
        },
        "fingerprint": {
            "file_id_calls": fingerprint.file_id_calls,
            "file_id_ms": nanos_to_ms(fingerprint.file_id_ns),
            "standard_calls": fingerprint.standard_calls,
            "standard_ms": nanos_to_ms(fingerprint.standard_ns),
            "basic_calls": fingerprint.basic_calls,
            "basic_ms": nanos_to_ms(fingerprint.basic_ns),
        },
        "output_bytes": emission.profile.output.len(),
        "output_equivalent": true,
    })
}

fn nanos_to_ms(nanoseconds: u64) -> f64 {
    std::time::Duration::from_nanos(nanoseconds).as_secs_f64() * 1_000.0
}

fn benchmark_identity() -> (String, String) {
    (
        std::env::var(BENCH_COMMIT_ENV).unwrap_or_else(|_| "unrecorded".to_owned()),
        std::env::var(BENCH_WORKTREE_ENV).unwrap_or_else(|_| "unrecorded".to_owned()),
    )
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
            let collect_worker_metrics = operation.starts_with("grep_lanes_");
            if collect_worker_metrics {
                grep::reset_worker_metrics();
            }
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
            let worker_metrics = collect_worker_metrics.then(|| {
                let metrics = grep::worker_metrics();
                json!({
                    "spawned": metrics.spawned,
                    "peak_active": metrics.peak_active,
                })
            });
            json!({
                "calls": calls,
                "elapsed_ms": elapsed * 1_000.0,
                "throughput_per_second": f64::from(u32::try_from(calls).expect("bounded concurrency")) / elapsed,
                "grep_workers": worker_metrics,
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
    let scale = f64::from(
        u32::try_from(files.div_ceil(1_000)).expect("supported fixture scale fits in u32"),
    );
    if operation.starts_with("glob") {
        return configured_limit(GLOB_P95_ENV, 250.0) * scale;
    }
    if operation.starts_with("grep_") {
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
    static CONFIGURED: OnceLock<Option<Vec<usize>>> = OnceLock::new();
    if let Some(configured) = CONFIGURED
        .get_or_init(|| {
            std::env::var(GREP_LANES_ENV).ok().map(|value| {
                let lanes = value
                    .split(',')
                    .map(str::trim)
                    .map(|value| value.parse::<usize>().expect("grep lane is an integer"))
                    .collect::<Vec<_>>();
                assert!(
                    !lanes.is_empty() && lanes.iter().all(|lane| GREP_LANES.contains(lane)),
                    "{GREP_LANES_ENV} accepts only comma-separated values from 1,2,4,8,16"
                );
                lanes
            })
        })
        .as_deref()
    {
        return configured;
    }
    if quick_mode() {
        &GREP_LANES[4..]
    } else {
        &GREP_LANES
    }
}

fn grep_mode() -> GrepMode {
    match std::env::var(GREP_MODE_ENV).as_deref() {
        Ok("content") | Err(std::env::VarError::NotPresent) => GrepMode::Content,
        Ok("files") => GrepMode::Files,
        Ok("count") => GrepMode::Count,
        Ok(value) => panic!("{GREP_MODE_ENV} accepts only content,files,count; got {value}"),
        Err(error) => panic!("{GREP_MODE_ENV} is not valid Unicode: {error}"),
    }
}

fn grep_sources() -> &'static [(&'static str, grep::GrepSourcePolicy)] {
    const DEFAULT: [(&str, grep::GrepSourcePolicy); 1] =
        [("hybrid", grep::GrepSourcePolicy::Hybrid)];
    static CONFIGURED: OnceLock<Option<Vec<(&'static str, grep::GrepSourcePolicy)>>> =
        OnceLock::new();
    CONFIGURED
        .get_or_init(|| {
            std::env::var(GREP_SOURCES_ENV).ok().map(|value| {
                let sources = value
                    .split(',')
                    .map(str::trim)
                    .map(|value| match value {
                        "hybrid" => ("hybrid", grep::GrepSourcePolicy::Hybrid),
                        "reader" => ("reader", grep::GrepSourcePolicy::Reader),
                        "file-never" => ("file-never", grep::GrepSourcePolicy::FileNever),
                        "mmap-always" => ("mmap-always", grep::GrepSourcePolicy::MmapAlways),
                        value if value.starts_with("mmap-threshold:") => {
                            let bytes = value["mmap-threshold:".len()..]
                                .parse::<u64>()
                                .ok()
                                .filter(|bytes| *bytes > 0)
                                .expect("mmap threshold is a positive byte count");
                            let name = Box::leak(value.to_owned().into_boxed_str());
                            (&*name, grep::GrepSourcePolicy::MmapThreshold(bytes))
                        }
                        _ => panic!(
                            "{GREP_SOURCES_ENV} accepts hybrid,reader,file-never,mmap-always,or \
                             mmap-threshold:<bytes>"
                        ),
                    })
                    .collect::<Vec<_>>();
                assert!(!sources.is_empty(), "{GREP_SOURCES_ENV} is empty");
                sources
            })
        })
        .as_deref()
        .unwrap_or(&DEFAULT)
}

fn pathname_reopen_variants() -> &'static [(&'static str, grep::PathnameReopenPolicy)] {
    const DEFAULT: [(&str, grep::PathnameReopenPolicy); 1] =
        [("off", grep::PathnameReopenPolicy::Off)];
    static CONFIGURED: OnceLock<Option<Vec<(&'static str, grep::PathnameReopenPolicy)>>> =
        OnceLock::new();
    CONFIGURED
        .get_or_init(|| {
            std::env::var(GREP_PATHNAME_REOPEN_ENV).ok().map(|value| {
                let variants = value
                    .split(',')
                    .map(str::trim)
                    .map(|value| match value {
                        "on" => ("on", grep::PathnameReopenPolicy::On),
                        "off" => ("off", grep::PathnameReopenPolicy::Off),
                        "parent-batch" => ("parent-batch", grep::PathnameReopenPolicy::ParentBatch),
                        _ => {
                            panic!("{GREP_PATHNAME_REOPEN_ENV} accepts only on,off,parent-batch")
                        }
                    })
                    .collect::<Vec<_>>();
                assert!(!variants.is_empty(), "{GREP_PATHNAME_REOPEN_ENV} is empty");
                variants
            })
        })
        .as_deref()
        .unwrap_or(&DEFAULT)
}

fn grep_traversals() -> &'static [(&'static str, grep::GrepTraversal)] {
    const DEFAULT: [(&str, grep::GrepTraversal); 3] = [
        ("serial", grep::GrepTraversal::Serial),
        ("parallel_256", grep::GrepTraversal::ParallelBatched),
        ("adaptive", grep::GrepTraversal::Adaptive),
    ];
    static CONFIGURED: OnceLock<Option<Vec<(&'static str, grep::GrepTraversal)>>> = OnceLock::new();
    CONFIGURED
        .get_or_init(|| {
            std::env::var(GREP_TRAVERSALS_ENV).ok().map(|value| {
                let traversals = value
                    .split(',')
                    .map(str::trim)
                    .map(|value| match value {
                        "serial" => ("serial", grep::GrepTraversal::Serial),
                        "parallel_256" => ("parallel_256", grep::GrepTraversal::ParallelBatched),
                        "adaptive" => ("adaptive", grep::GrepTraversal::Adaptive),
                        "serial_prefix" => {
                            ("serial_prefix", grep::GrepTraversal::SerialLiteralPrefix)
                        }
                        "parallel_256_prefix" => (
                            "parallel_256_prefix",
                            grep::GrepTraversal::ParallelBatchedLiteralPrefix,
                        ),
                        _ => panic!(
                            "{GREP_TRAVERSALS_ENV} accepts only serial,parallel_256,adaptive,\
                             serial_prefix,parallel_256_prefix"
                        ),
                    })
                    .collect::<Vec<_>>();
                assert!(!traversals.is_empty(), "{GREP_TRAVERSALS_ENV} is empty");
                traversals
            })
        })
        .as_deref()
        .unwrap_or(&DEFAULT)
}

fn grep_workload() -> GrepWorkload {
    static WORKLOAD: OnceLock<GrepWorkload> = OnceLock::new();
    *WORKLOAD.get_or_init(|| {
        let value = std::env::var(GREP_WORKLOAD_ENV).unwrap_or_else(|_| "legacy".to_owned());
        match value.as_str() {
            "legacy" => GrepWorkload {
                name: "legacy",
                file_size: GrepFileSize::Legacy,
                density: MatchDensity::Legacy,
            },
            "1k-none" => GrepWorkload::matrix("1k-none", GrepFileSize::OneKiB, MatchDensity::None),
            "1k-rare" => GrepWorkload::matrix("1k-rare", GrepFileSize::OneKiB, MatchDensity::Rare),
            "1k-dense" => {
                GrepWorkload::matrix("1k-dense", GrepFileSize::OneKiB, MatchDensity::Dense)
            }
            "64k-none" => {
                GrepWorkload::matrix("64k-none", GrepFileSize::SixtyFourKiB, MatchDensity::None)
            }
            "64k-rare" => {
                GrepWorkload::matrix("64k-rare", GrepFileSize::SixtyFourKiB, MatchDensity::Rare)
            }
            "64k-dense" => {
                GrepWorkload::matrix("64k-dense", GrepFileSize::SixtyFourKiB, MatchDensity::Dense)
            }
            "4m-none" => GrepWorkload::matrix("4m-none", GrepFileSize::FourMiB, MatchDensity::None),
            "4m-rare" => GrepWorkload::matrix("4m-rare", GrepFileSize::FourMiB, MatchDensity::Rare),
            "4m-dense" => {
                GrepWorkload::matrix("4m-dense", GrepFileSize::FourMiB, MatchDensity::Dense)
            }
            _ => panic!(
                "{GREP_WORKLOAD_ENV} accepts legacy, 1k-none, 1k-rare, 1k-dense, \
                 64k-none, 64k-rare, 64k-dense, 4m-none, 4m-rare, or 4m-dense"
            ),
        }
    })
}

impl GrepWorkload {
    const fn matrix(name: &'static str, file_size: GrepFileSize, density: MatchDensity) -> Self {
        Self {
            name,
            file_size,
            density,
        }
    }

    fn selected_files(self, files: usize) -> usize {
        if matches!(self.file_size, GrepFileSize::Legacy) {
            return files;
        }
        std::env::var(GREP_SELECTED_FILES_ENV)
            .unwrap_or_else(|_| {
                panic!("{GREP_SELECTED_FILES_ENV} is required for non-legacy grep workloads")
            })
            .parse::<usize>()
            .ok()
            .filter(|selected| (1..=files).contains(selected))
            .unwrap_or_else(|| {
                panic!("{GREP_SELECTED_FILES_ENV} must be an integer from 1 to {files}")
            })
    }

    fn glob(self) -> &'static str {
        match self.file_size {
            GrepFileSize::Legacy => "**/*.rs",
            GrepFileSize::OneKiB | GrepFileSize::SixtyFourKiB | GrepFileSize::FourMiB => {
                "**/*.selected.rs"
            }
        }
    }

    fn content(self, index: usize) -> String {
        if matches!(self.file_size, GrepFileSize::Legacy) {
            return format!("pub fn fixture_{index}() {{}}\nneedle-{index}\n");
        }
        let target = match self.file_size {
            GrepFileSize::Legacy => unreachable!(),
            GrepFileSize::OneKiB => 1_024,
            GrepFileSize::SixtyFourKiB => 64 * 1_024,
            GrepFileSize::FourMiB => 4 * 1024 * 1024,
        };
        let matching = match self.density {
            MatchDensity::Legacy => unreachable!(),
            MatchDensity::None => false,
            MatchDensity::Rare => index.is_multiple_of(100),
            MatchDensity::Dense => true,
        };
        let line = if matches!(self.density, MatchDensity::Dense) {
            format!("needle-{index}\n")
        } else {
            format!("ordinary-{index}\n")
        };
        let mut content = String::with_capacity(target);
        if matching && !matches!(self.density, MatchDensity::Dense) {
            writeln!(content, "needle-{index}").expect("write fixture match");
        }
        while content.len().saturating_add(line.len()) <= target {
            content.push_str(&line);
        }
        content.extend(std::iter::repeat_n(
            'x',
            target.saturating_sub(content.len()),
        ));
        content
    }
}

fn concurrency_levels() -> &'static [usize] {
    if quick_mode() && std::env::var_os(BENCH_QUICK_CONCURRENT_ENV).is_none_or(|value| value != "1")
    {
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
