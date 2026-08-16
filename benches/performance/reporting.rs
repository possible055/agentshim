use super::configuration::{concurrency_levels, grep_workload, percentile};
use super::{
    BENCH_COMMIT_ENV, BENCH_QUICK_ENV, BENCH_WARM_SAMPLES_ENV, BENCH_WORKTREE_ENV, GLOB_P95_ENV,
    GREP_COMPACT_PROFILE_ENV, GREP_P95_ENV, Instant, READ_P95_ENV, WARM_SAMPLES, grep, json, read,
};

pub(super) struct GrepProfileEmission<'a> {
    pub(super) scope: &'a str,
    pub(super) files: usize,
    pub(super) traversal: &'a str,
    pub(super) source: &'a str,
    pub(super) pathname_reopen: &'a str,
    pub(super) expected: &'a str,
    pub(super) profile: grep::ProfiledGrep,
    pub(super) fingerprint: read::FingerprintMetrics,
    pub(super) mmap: (usize, usize),
}

pub(super) fn emit_grep_profile(emission: &GrepProfileEmission<'_>) {
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
                "sort": std::env::var("AGENTSHIM_BENCH_GREP_SORT")
                    .unwrap_or_else(|_| "heapsort".to_owned()),
                "lanes": stages.lanes,
                "candidate_count": stages.candidate_count,
                "searched_candidates": stages.searched_candidates,
                "reduced_candidates": stages.reduced_candidates,
                "scan_complete": stages.scan_complete,
                "matched_candidates": stages.matched_candidates,
                "candidate_retained_memory_bytes": stages.candidate_retained_memory_bytes,
                "candidate_vec_capacity": stages.candidate_vec_capacity,
                "candidate_soft_target_crossings": stages.candidate_soft_target_crossings,
                "speculative_lease_requested_bytes": stages.speculative_lease_requested_bytes,
                "speculative_lease_granted_bytes": stages.speculative_lease_granted_bytes,
                "capture_exact_retries": stages.capture_exact_retries,
                "heap_limit_retries": stages.heap_limit_retries,
                "retry_successes": stages.retry_successes,
                "retry_ceiling_bytes": stages.retry_ceiling_bytes,
                "legacy_stream_files": stages.legacy_stream_files,
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
                "candidate_policy": std::env::var("AGENTSHIM_BENCH_GREP_CANDIDATE_POLICY")
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

// Keeping the complete benchmark schema together makes emitted fields auditable as one contract.
#[allow(clippy::too_many_lines)]
pub(super) fn full_grep_profile_json(emission: &GrepProfileEmission<'_>) -> serde_json::Value {
    let stages = &emission.profile.timings;
    let fingerprint = emission.fingerprint;
    let mmap = emission.mmap;
    let limits = agentshim::RuntimeLimits::for_tests(stages.lanes);
    let mut profile = json!({
        "benchmark": "grep_profile",
        "binary_commit": benchmark_identity().0,
        "binary_worktree": benchmark_identity().1,
        "memory_policy": "soft_gate",
        "grep_memory_bytes": limits.grep_memory_bytes,
        "shared_memory_bytes": limits.memory_bytes,
        "candidate_policy": std::env::var("AGENTSHIM_BENCH_GREP_CANDIDATE_POLICY")
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
        "sort": std::env::var("AGENTSHIM_BENCH_GREP_SORT")
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
            "legacy_stream": stages.legacy_stream_files,
            "mmap_requested": stages.mmap_requested_files,
            "mmap_selected": mmap.0,
            "mmap_fallback": mmap.1,
            "pathname_reopens": stages.pathname_reopens,
        },
        "bytes": {
            "render_copy": stages.render_copy_bytes,
            "speculative_lease_requested": stages.speculative_lease_requested_bytes,
            "speculative_lease_granted": stages.speculative_lease_granted_bytes,
        },
        "retries": {
            "capture_exact": stages.capture_exact_retries,
            "heap_limit": stages.heap_limit_retries,
            "successes": stages.retry_successes,
            "ceiling_bytes": stages.retry_ceiling_bytes,
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
    });
    profile["reduced_candidates"] = stages.reduced_candidates.into();
    profile["scan_complete"] = stages.scan_complete.into();
    profile
}

pub(super) fn nanos_to_ms(nanoseconds: u64) -> f64 {
    std::time::Duration::from_nanos(nanoseconds).as_secs_f64() * 1_000.0
}

pub(super) fn benchmark_identity() -> (String, String) {
    (
        std::env::var(BENCH_COMMIT_ENV).unwrap_or_else(|_| "unrecorded".to_owned()),
        std::env::var(BENCH_WORKTREE_ENV).unwrap_or_else(|_| "unrecorded".to_owned()),
    )
}

pub(super) fn measure(
    scope: &str,
    operation: &str,
    files: usize,
    execute: impl Fn() -> String + Sync,
) {
    measure_with(scope, operation, files, execute, |left, right| {
        left == right
    });
}

pub(super) fn measure_with(
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

    let warm_samples = warm_samples();
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
                    "active": metrics.active,
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

pub(super) fn p95_limit(operation: &str, files: usize) -> f64 {
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

pub(super) fn configured_limit(name: &str, default: f64) -> f64 {
    let value = std::env::var(name).map_or(default, |value| {
        value
            .parse::<f64>()
            .unwrap_or_else(|_| panic!("{name} must be a number"))
    });
    assert!(value.is_finite() && value > 0.0, "{name} must be positive");
    value
}

pub(super) fn quick_mode() -> bool {
    std::env::var_os(BENCH_QUICK_ENV).is_some_and(|value| value == "1")
}

pub(super) fn warm_samples() -> usize {
    if quick_mode() {
        return 1;
    }
    std::env::var(BENCH_WARM_SAMPLES_ENV).map_or(WARM_SAMPLES, |value| {
        value
            .parse::<usize>()
            .ok()
            .filter(|samples| (1..=100).contains(samples))
            .unwrap_or_else(|| panic!("{BENCH_WARM_SAMPLES_ENV} must be from 1 to 100"))
    })
}
