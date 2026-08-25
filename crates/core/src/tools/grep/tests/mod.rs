use std::{fs, path::Path, sync::Arc};

use tokio_util::sync::CancellationToken;

use crate::output::SkipReason;
#[cfg(feature = "bench-internals")]
use crate::tools::grep::execute_profiled;
use crate::tools::grep::{
    CandidateCollection, CaseMode, GrepBenchmarkVariant, GrepError, GrepMemoryPolicy, GrepMode,
    GrepRequest, GrepSourcePolicy, GrepTraversal, PAGE_MEMORY_BYTES, Page, PathnameReopenPolicy,
    SearchPlan, build_matcher, candidate, execute, execute_with_memory_budget,
    execute_with_traversal, execute_with_variant, render, render_with_budget, search_file_with,
};
use crate::{
    path::{FileAccess, ReadScope, RepositoryRoot},
    runtime::{MIN_TOOL_MEMORY_BYTES, MemoryReservation, RuntimeConfig, RuntimeResources},
};

mod encoding;
mod paging;
mod search;

fn request(pattern: &str) -> GrepRequest {
    GrepRequest {
        pattern: pattern.to_owned(),
        path: None,
        glob: Some("**/*.rs".to_owned()),
        mode: None,
        fixed_strings: None,
        case: None,
        context_lines: None,
        offset: None,
        limit: None,
        include_ignored: None,
        encoding: None,
        fallback_encoding: None,
    }
}

fn fixture() -> (tempfile::TempDir, Arc<FileAccess>) {
    let fixture = tempfile::tempdir().expect("fixture");
    fs::create_dir(fixture.path().join("src")).expect("src");
    fs::write(
        fixture.path().join("src/a.rs"),
        "before\nNeedle needle\nafter\n",
    )
    .expect("a");
    fs::write(fixture.path().join("src/b.rs"), "needle\nnone\nneedle\n").expect("b");
    fs::write(fixture.path().join("ignored.rs"), "needle\n").expect("ignored");
    fs::write(fixture.path().join(".gitignore"), "ignored.rs\n").expect("gitignore");
    let root = root_at(fixture.path());
    (fixture, root)
}

fn root_at(dir: &Path) -> Arc<FileAccess> {
    Arc::new(FileAccess::new(
        Arc::new(RepositoryRoot::open(dir).expect("root")),
        ReadScope::Normal,
    ))
}

fn content_plan() -> SearchPlan {
    SearchPlan {
        memory: GrepMemoryPolicy::new(256 * 1024 * 1024),
        mode: GrepMode::Content,
        context: 0,
        probe: 10,
        skip: 0,
        allow_early_stop: false,
        encoding: None,
        fallback_encoding: None,
    }
}

fn result_lines(output: &str) -> Vec<&str> {
    output
        .lines()
        .filter(|line| {
            !line.starts_with("Partial:") && *line != "Complete." && !line.starts_with("Skipped")
        })
        .collect()
}

fn sorted_result_lines(output: &str) -> Vec<String> {
    let mut lines = result_lines(output)
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    lines.sort_unstable();
    lines
}

#[test]
fn an_empty_gitignore_filtered_search_recommends_the_retry_flag() {
    let fixture = tempfile::tempdir().expect("fixture");
    fs::write(fixture.path().join(".gitignore"), "hidden.rs\n").expect("ignore file");
    fs::write(fixture.path().join("hidden.rs"), "needle").expect("hidden source");
    let root = root_at(fixture.path());
    let cancellation = CancellationToken::new();
    let mut query = request("needle");
    query.fixed_strings = Some(true);

    query.include_ignored = Some(false);
    let filtered = execute(&root, &query, 1, &cancellation).expect("filtered search");
    assert!(filtered.contains("No matches."));
    assert!(filtered.contains("include_ignored=true"));

    query.include_ignored = Some(true);
    let included = execute(&root, &query, 1, &cancellation).expect("included search");
    assert!(included.contains("needle"));
    assert!(!included.contains("include_ignored=true"));
}

#[test]
fn empty_results_distinguish_an_empty_search_from_an_empty_page() {
    let (_fixture, root) = fixture();
    let cancellation = CancellationToken::new();
    let mut query = request("absent-value");
    query.fixed_strings = Some(true);

    assert_eq!(
        execute(&root, &query, 1, &cancellation).expect("empty search"),
        "No matches."
    );
    query.offset = Some(3);
    assert_eq!(
        execute(&root, &query, 1, &cancellation).expect("empty page"),
        "No results at offset=3."
    );
}

#[test]
fn grep_results_are_deterministic_across_workers_and_glob_settings() {
    let (_fixture, root) = fixture();
    let cancellation = CancellationToken::new();
    for glob in [Some("**/*.rs".to_owned()), None] {
        let mut query = request("needle");
        query.glob = glob;
        query.fixed_strings = Some(true);
        query.case = Some(CaseMode::Insensitive);
        query.context_lines = Some(1);
        let baseline = execute(&root, &query, 1, &cancellation).expect("serial grep");
        assert!(baseline.contains("src/a.rs"));
        assert!(baseline.contains("-1-before"));
        assert!(baseline.contains("ignored.rs"));
        let expected = sorted_result_lines(&baseline);
        for workers in [2, 4, 8, 16] {
            let output = execute(&root, &query, workers, &cancellation).expect("parallel grep");
            assert_eq!(
                sorted_result_lines(&output),
                expected,
                "glob={:?}",
                query.glob
            );
        }
    }
}

#[test]
fn grep_pipelined_search_result_set_stable_across_workers() {
    let (_fixture, root) = fixture();
    let mut query = request("needle");
    query.fixed_strings = Some(true);
    query.mode = Some(GrepMode::Files);
    query.limit = Some(10);
    let cancellation = CancellationToken::new();
    let serial =
        sorted_result_lines(&execute(&root, &query, 1, &cancellation).expect("serial grep"));
    let parallel =
        sorted_result_lines(&execute(&root, &query, 4, &cancellation).expect("parallel grep"));
    assert_eq!(parallel, serial);
}

#[test]
fn grep_early_stop_returns_limit_results() {
    let (_fixture, root) = fixture();
    let mut query = request("needle");
    query.fixed_strings = Some(true);
    query.mode = Some(GrepMode::Files);
    query.limit = Some(1);

    let output = execute(&root, &query, 4, &CancellationToken::new()).expect("early stop");

    assert_eq!(result_lines(&output).len(), 1);
}

#[test]
fn grep_early_stop_truncates_traversal() {
    let (_fixture, root) = fixture();
    let mut query = request("needle");
    query.fixed_strings = Some(true);
    query.mode = Some(GrepMode::Files);
    query.limit = Some(1);

    let output = execute(&root, &query, 4, &CancellationToken::new()).expect("early stop");

    assert!(output.contains("Partial: next_offset="));
    assert!(!output.contains("Complete."));
}

#[test]
fn grep_count_mode_no_early_stop() {
    let (_fixture, root) = fixture();
    let mut query = request("needle");
    query.fixed_strings = Some(true);
    query.mode = Some(GrepMode::Count);
    query.limit = Some(10);

    let output = execute(&root, &query, 4, &CancellationToken::new()).expect("count grep");

    assert!(!output.contains("Partial: next_offset="));
}

#[test]
fn grep_offset_best_effort_does_not_crash() {
    let (_fixture, root) = fixture();
    let mut query = request("needle");
    query.fixed_strings = Some(true);
    query.mode = Some(GrepMode::Files);
    query.offset = Some(1);
    query.limit = Some(1);

    let output = execute(&root, &query, 4, &CancellationToken::new()).expect("offset grep");

    assert!(result_lines(&output).len() <= 1);
}

#[test]
fn token_dense_matches_preserve_pagination_and_model_budget() {
    let fixture = tempfile::tempdir().expect("fixture");
    fs::create_dir(fixture.path().join("src")).expect("src");
    let dense = format!("{}\n", " x".repeat(12)).repeat(750);
    fs::write(fixture.path().join("src/dense.rs"), dense).expect("dense source");
    let root = root_at(fixture.path());
    let cancellation = CancellationToken::new();
    let mut query = request("x");
    query.fixed_strings = Some(true);
    query.limit = Some(750);

    let output = execute(&root, &query, 1, &cancellation).expect("bounded dense grep");

    assert!(matches!(
        crate::output::TokenGate::project_tool_output(
            &crate::output::TestCallBudget::default(),
            &output,
            0,
            &cancellation
        ),
        crate::output::ProjectionDecision::Fits(_)
    ));
    assert!(output.contains("Partial: next_offset="));
}

#[test]
fn partial_pages_keep_the_shown_offset_under_burst_and_item_ceilings() {
    let query = request("needle");
    let mut page = Page::new(
        &query,
        crate::traversal::TraversalSummary::default(),
        false,
        false,
    );
    page.mark_complete();
    for index in 0..80 {
        page.push_entry(
            format!("src/{index:02}.rs:{index}:{}", " x".repeat(20)),
            Some(format!(
                "src/{index:02}.rs:{index}:[line text omitted: exceeds output budget]"
            )),
        );
    }
    let cancellation = CancellationToken::new();
    let burst_512 = crate::output::TestCallBudget {
        ceiling: 512,
        ..crate::output::TestCallBudget::default()
    };
    let output =
        render_with_budget(&query, &page, &cancellation, &burst_512).expect("512-token grep page");
    let next = output
        .lines()
        .find_map(|line| {
            line.strip_prefix("Partial: next_offset=")?
                .trim_end_matches('.')
                .parse::<usize>()
                .ok()
        })
        .expect("partial cursor");
    assert_eq!(next, result_lines(&output).len());
    assert!(output.fits_call_budget(&burst_512, &cancellation));
}

#[cfg(feature = "bench-internals")]
#[test]
fn profiled_execution_preserves_output_and_records_wall_stages() {
    let (_fixture, root) = fixture();
    let mut query = request("needle");
    query.fixed_strings = Some(true);
    query.mode = Some(GrepMode::Count);
    let cancellation = CancellationToken::new();
    let expected = execute(&root, &query, 4, &cancellation).expect("grep");
    let profile = execute_profiled(&root, &query, 4, &cancellation).expect("profiled grep");

    assert_eq!(
        sorted_result_lines(&profile.output),
        sorted_result_lines(&expected)
    );
    assert_eq!(profile.timings.candidate_count, 3);
    assert_eq!(profile.timings.lanes, 3);
    assert_eq!(profile.timings.reduced_candidates, 3);
    assert!(profile.timings.scan_complete);
    let search_ns = profile
        .timings
        .pipeline_ns
        .max(profile.timings.search_wall_ns);
    let sequential_ns = profile
        .timings
        .setup_ns
        .saturating_add(profile.timings.candidate_traversal_ns)
        .saturating_add(profile.timings.candidate_sort_ns)
        .saturating_add(search_ns)
        .saturating_add(profile.timings.render_ns);
    assert!(profile.timings.total_ns >= sequential_ns);
    assert!(profile.timings.pipeline_ns > 0);
    assert!(profile.timings.search_wall_ns > 0);

    let mut partial_query = request("needle");
    partial_query.fixed_strings = Some(true);
    partial_query.limit = Some(1);
    let partial =
        execute_profiled(&root, &partial_query, 4, &cancellation).expect("profiled partial grep");
    assert!(partial.output.contains("Partial: next_offset=1."));
    assert!(!partial.timings.scan_complete);
    assert!(partial.timings.reduced_candidates < partial.timings.candidate_count);
}

#[test]
fn serial_and_parallel_candidate_traversal_are_equivalent() {
    let (_fixture, root) = fixture();
    let mut query = request("needle");
    query.fixed_strings = Some(true);
    query.mode = Some(GrepMode::Count);
    let cancellation = CancellationToken::new();
    let serial = execute_with_traversal(&root, &query, 4, &cancellation, GrepTraversal::Serial)
        .expect("serial grep");
    let parallel = execute_with_traversal(
        &root,
        &query,
        4,
        &cancellation,
        GrepTraversal::ParallelBatched,
    )
    .expect("parallel grep");

    assert_eq!(sorted_result_lines(&parallel), sorted_result_lines(&serial));
}

#[test]
fn literal_prefix_grep_preserves_serial_and_parallel_output() {
    let (fixture, root) = fixture();
    fs::create_dir(fixture.path().join("other")).expect("other");
    fs::write(fixture.path().join("other/c.rs"), "needle\n").expect("c");
    let mut query = request("needle");
    query.fixed_strings = Some(true);
    query.mode = Some(GrepMode::Count);
    query.glob = Some("src/*.rs".to_owned());
    let cancellation = CancellationToken::new();
    let expected = sorted_result_lines(
        &execute_with_traversal(&root, &query, 4, &cancellation, GrepTraversal::Serial)
            .expect("serial grep"),
    );

    for traversal in [
        GrepTraversal::SerialLiteralPrefix,
        GrepTraversal::ParallelBatchedLiteralPrefix,
    ] {
        let output = execute_with_traversal(&root, &query, 4, &cancellation, traversal)
            .expect("literal prefix grep");
        assert_eq!(sorted_result_lines(&output), expected);
    }
}

#[test]
fn candidate_memory_limit_rejects_the_first_byte_over_the_limit() {
    let (_fixture, root) = fixture();
    let path = root.resolve(Path::new("src/a.rs")).expect("candidate path");
    let candidate = candidate(path).expect("candidate");
    let mut oracle = CandidateCollection::new(GrepMemoryPolicy::candidate_only(usize::MAX), None);
    oracle.admit(candidate.clone()).expect("oracle admission");
    let limit = oracle.estimated_retained_bytes - 1;
    let mut limited = CandidateCollection::new(GrepMemoryPolicy::candidate_only(limit), None);

    assert!(matches!(
        limited.admit(candidate),
        Err(GrepError::CandidateMemory)
    ));
}

#[test]
fn candidate_global_memory_pressure_is_retryable_and_releases_permits() {
    let (_fixture, root) = fixture();
    let path = root.resolve(Path::new("src/a.rs")).expect("candidate path");
    let candidate = candidate(path).expect("candidate");
    let mut config = RuntimeConfig::for_tests(1);
    config.memory_bytes = MIN_TOOL_MEMORY_BYTES;
    let resources = RuntimeResources::new(config);
    let initial = resources
        .try_reserve_memory(1024)
        .expect("initial reservation");
    let reservation = MemoryReservation::from_initial(&resources, initial, 1024);
    let pressure = resources
        .try_reserve_memory(MIN_TOOL_MEMORY_BYTES - 1024)
        .expect("competing reservation");
    let mut collection = CandidateCollection::new(
        GrepMemoryPolicy::candidate_only(usize::MAX),
        Some(reservation),
    );

    assert!(matches!(
        collection.admit(candidate),
        Err(GrepError::MemoryBusy)
    ));
    drop(collection);
    drop(pressure);
    assert!(
        resources
            .try_reserve_memory(MIN_TOOL_MEMORY_BYTES)
            .is_some()
    );
}

#[test]
fn candidate_collection_holds_its_memory_reservation_until_drop() {
    let (_fixture, root) = fixture();
    let path = root.resolve(Path::new("src/a.rs")).expect("candidate path");
    let candidate = candidate(path).expect("candidate");
    let mut config = RuntimeConfig::for_tests(1);
    config.memory_bytes = 32 * 1024 * 1024;
    config.grep_memory_bytes = config.memory_bytes;
    let resources = RuntimeResources::new(config);
    let policy = GrepMemoryPolicy::new(config.grep_memory_bytes);
    let initial = resources
        .try_reserve_memory(policy.base_reservation_bytes())
        .expect("base reservation");
    let reservation =
        MemoryReservation::from_initial(&resources, initial, policy.base_reservation_bytes());
    let mut collection = CandidateCollection::new(policy, Some(reservation));
    let available_before_admit = resources.available_memory_bytes();
    collection.admit(candidate).expect("candidate admission");

    assert!(
        resources.available_memory_bytes() < available_before_admit,
        "admitting a candidate must grow the lease"
    );
    drop(collection);
    assert_eq!(resources.available_memory_bytes(), config.memory_bytes);
}
