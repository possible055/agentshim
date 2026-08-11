use super::configuration::{
    grep_lanes, grep_mode, grep_sources, grep_traversals, grep_workload, pathname_reopen_variants,
    percentile,
};
use super::fixtures::{mmap_trace, reset_mmap_trace};
use super::reporting::{
    GrepProfileEmission, emit_grep_profile, measure, nanos_to_ms, warm_samples,
};
use super::{
    Arc, CancellationToken, FileAccess, GLOB_PATTERN_ENV, GLOB_PROFILE_ONLY_ENV,
    GLOB_TRAVERSALS_ENV, GLOB_VARIANTS_ONLY_ENV, GREP_GLOB_ENV, GREP_PROFILE_ENV,
    GREP_PROFILE_ONLY_ENV, GREP_VARIANTS_ONLY_ENV, GlobRequest, GlobTraversal, GrepRequest,
    Instant, OPEN_BATCH_ONLY_ENV, OnceLock, OpenReadStrategy, Path, READ_LINE_COUNT_ENV,
    READ_ONLY_ENV, READ_START_LINE_ENV, ReadRequest, ResolvedPath, glob, grep, json,
    open_read_batches, read,
};

pub(super) fn benchmark_tools(
    scope: &str,
    files: usize,
    access: &Arc<FileAccess>,
    directory: &str,
    read_path: &str,
) {
    let cancellation = CancellationToken::new();
    if std::env::var_os(READ_ONLY_ENV).is_some_and(|value| value == "1") {
        benchmark_read(access, read_path, &cancellation, files, scope);
        return;
    }
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
    benchmark_read(access, read_path, &cancellation, files, scope);
}

pub(super) fn benchmark_read(
    access: &Arc<FileAccess>,
    read_path: &str,
    cancellation: &CancellationToken,
    files: usize,
    scope: &str,
) {
    measure(scope, "read", files, || {
        read::execute(
            access,
            &ReadRequest {
                path: read_path.to_owned(),
                start_line: Some(read_benchmark_value(READ_START_LINE_ENV, 1)),
                line_count: Some(read_benchmark_value(READ_LINE_COUNT_ENV, 2_000)),
                encoding: None,
                pdf_mode: None,
                pages: None,
                pdf_text_offset: None,
                pdf_source_id: None,
            },
            cancellation,
        )
        .expect("read benchmark")
    });
}

pub(super) fn read_benchmark_value(name: &str, default: usize) -> usize {
    std::env::var(name).map_or(default, |value| {
        value
            .parse::<usize>()
            .ok()
            .filter(|value| *value > 0)
            .unwrap_or_else(|| panic!("{name} must be a positive integer"))
    })
}

pub(super) fn benchmark_open_batches(
    scope: &str,
    files: usize,
    access: &FileAccess,
    directory: &str,
) {
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

pub(super) fn measure_open_batches(
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

    let warm_samples = warm_samples();
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

pub(super) fn glob_request(directory: &str) -> GlobRequest {
    GlobRequest {
        pattern: std::env::var(GLOB_PATTERN_ENV).unwrap_or_else(|_| "**/*.rs".to_owned()),
        path: Some(directory.to_owned()),
        include_ignored: None,
        entry_type: None,
        offset: None,
        limit: Some(1_000),
    }
}

pub(super) fn profile_glob(
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

pub(super) fn grep_request(directory: &str, files: usize) -> GrepRequest {
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

pub(super) fn glob_traversals() -> &'static [(&'static str, glob::GlobTraversal)] {
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

pub(super) fn profile_grep(
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
