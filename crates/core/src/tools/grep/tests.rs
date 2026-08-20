#[cfg(test)]
mod tests {
    use std::{fs, path::Path, sync::Arc};

    use tokio_util::sync::CancellationToken;

    use crate::output::SkipReason;
    #[cfg(feature = "bench-internals")]
    use crate::tools::grep::execute_profiled;
    use crate::tools::grep::{
        CandidateCollection, CandidatePolicy, CaseMode, GrepBenchmarkVariant, GrepError,
        GrepMemoryPolicy, GrepMode, GrepRequest, GrepSourcePolicy, GrepTraversal,
        PAGE_MEMORY_BYTES, Page, PathnameReopenPolicy, SearchPlan, build_matcher, candidate,
        execute, execute_with_memory_budget, execute_with_traversal, execute_with_variant, render,
        render_with_budget, search_file, search_file_with_hook, search_file_with_variant_hook,
    };
    use crate::{
        path::{FileAccess, ReadScope, RepositoryRoot},
        runtime::{MIN_TOOL_MEMORY_BYTES, MemoryReservation, RuntimeConfig, RuntimeResources},
        traversal::TraversalSummary,
    };

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
        let root = Arc::new(FileAccess::new(
            Arc::new(RepositoryRoot::open(fixture.path()).expect("root")),
            ReadScope::Normal,
        ));
        (fixture, root)
    }

    /// A legacy-encoded file used to answer "No matches." for any pattern whose UTF-8
    /// bytes could not appear in it, which is every CJK pattern. The bytes now get decoded
    /// before the matcher sees them.
    #[test]
    fn legacy_encoded_files_are_searched_after_decoding() {
        let fixture = tempfile::tempdir().expect("fixture");
        fs::write(
            fixture.path().join("big5.txt"),
            encoding_rs::BIG5
                .encode("繁體中文測試資料\n第二行內容足夠辨識\n")
                .0,
        )
        .expect("big5 source");
        fs::write(
            fixture.path().join("gbk.txt"),
            encoding_rs::GBK
                .encode("简体中文测试数据\n第二行内容足够识别\n")
                .0,
        )
        .expect("gbk source");
        let root = Arc::new(FileAccess::new(
            Arc::new(RepositoryRoot::open(fixture.path()).expect("root")),
            ReadScope::Normal,
        ));
        let cancellation = CancellationToken::new();

        let mut query = request("繁體");
        query.glob = None;
        query.fixed_strings = Some(true);
        let traditional = execute(&root, &query, 1, &cancellation).expect("big5 search");
        assert!(
            traditional.contains("big5.txt"),
            "expected a Big5 hit, got {traditional}"
        );

        let mut query = request("简体");
        query.glob = None;
        query.fixed_strings = Some(true);
        let simplified = execute(&root, &query, 1, &cancellation).expect("gbk search");
        assert!(
            simplified.contains("gbk.txt"),
            "expected a GBK hit, got {simplified}"
        );
    }

    #[test]
    fn large_legacy_file_streams_without_a_decoded_whole_file_limit() {
        let fixture = tempfile::tempdir().expect("fixture");
        let text = "繁體中文測試資料內容\n".repeat(350_000);
        let encoded = encoding_rs::BIG5.encode(&text).0;
        assert!(text.len() > 8 * 1024 * 1024);
        fs::write(fixture.path().join("large-big5.txt"), encoded).expect("large Big5 source");
        let root = Arc::new(FileAccess::new(
            Arc::new(RepositoryRoot::open(fixture.path()).expect("root")),
            ReadScope::Normal,
        ));
        let mut query = request("繁體");
        query.path = Some("large-big5.txt".to_owned());
        query.glob = None;
        query.mode = Some(GrepMode::Files);
        query.fixed_strings = Some(true);
        query.encoding = Some("big5".to_owned());

        let output = execute(&root, &query, 1, &CancellationToken::new())
            .expect("large streaming Big5 search");
        assert!(
            output.contains("large-big5.txt"),
            "expected hit, got {output}"
        );
    }

    #[test]
    fn malformed_legacy_tail_invalidates_an_earlier_match() {
        let fixture = tempfile::tempdir().expect("fixture");
        let mut encoded = encoding_rs::BIG5
            .encode("繁體中文測試資料\n")
            .0
            .into_owned();
        encoded.push(0x81);
        fs::write(fixture.path().join("malformed-big5.txt"), encoded)
            .expect("malformed Big5 source");
        let root = Arc::new(FileAccess::new(
            Arc::new(RepositoryRoot::open(fixture.path()).expect("root")),
            ReadScope::Normal,
        ));
        let mut query = request("繁體");
        query.path = Some("malformed-big5.txt".to_owned());
        query.glob = None;
        query.fixed_strings = Some(true);
        query.encoding = Some("big5".to_owned());

        assert!(matches!(
            execute(&root, &query, 1, &CancellationToken::new()),
            Err(GrepError::Unsearchable(SkipReason::Undecodable))
        ));
    }

    #[test]
    fn bom_takes_precedence_over_explicit_encoding() {
        let fixture = tempfile::tempdir().expect("fixture");
        let mut encoded = vec![0xFF, 0xFE];
        for unit in "繁體中文\n".encode_utf16() {
            encoded.extend_from_slice(&unit.to_le_bytes());
        }
        fs::write(fixture.path().join("utf16.txt"), encoded).expect("UTF-16 source");
        let root = Arc::new(FileAccess::new(
            Arc::new(RepositoryRoot::open(fixture.path()).expect("root")),
            ReadScope::Normal,
        ));
        let mut query = request("繁體");
        query.path = Some("utf16.txt".to_owned());
        query.glob = None;
        query.fixed_strings = Some(true);
        query.encoding = Some("big5".to_owned());

        let output = execute(&root, &query, 1, &CancellationToken::new())
            .expect("BOM-selected UTF-16 search");
        assert!(
            output.contains("utf16.txt:1:繁體中文"),
            "expected hit, got {output}"
        );
    }

    /// An encoding detection cannot resolve must be reported, and the report must name the
    /// argument that makes the file reachable.
    #[test]
    fn undecodable_files_are_reported_with_a_fallback_hint() {
        let fixture = tempfile::tempdir().expect("fixture");
        fs::write(
            fixture.path().join("sparse.txt"),
            encoding_rs::BIG5.encode("fn 計算() {\n").0,
        )
        .expect("sparse big5 source");
        fs::write(fixture.path().join("plain.txt"), "計算\n").expect("utf8 source");
        let root = Arc::new(FileAccess::new(
            Arc::new(RepositoryRoot::open(fixture.path()).expect("root")),
            ReadScope::Normal,
        ));
        let cancellation = CancellationToken::new();
        let mut query = request("計算");
        query.glob = None;
        query.fixed_strings = Some(true);

        let reported = execute(&root, &query, 1, &cancellation).expect("search");
        assert!(
            reported.contains("undecodable"),
            "undecodable files must be listed, got {reported}"
        );
        assert!(
            reported.contains("fallback_encoding"),
            "the report must name the argument that reaches them, got {reported}"
        );

        query.fallback_encoding = Some("big5".to_owned());
        let recovered = execute(&root, &query, 1, &cancellation).expect("fallback search");
        assert!(
            recovered.contains("sparse.txt"),
            "fallback_encoding must reach the file, got {recovered}"
        );
        assert!(
            !recovered.contains("fallback_encoding="),
            "the hint must not repeat once the argument is supplied"
        );
    }

    #[test]
    fn fallback_encoding_reaches_legacy_text_after_a_long_ascii_header() {
        let fixture = tempfile::tempdir().expect("fixture");
        let mut encoded = vec![b'a'; 9 * 1024];
        encoded.push(b'\n');
        encoded.extend_from_slice(&encoding_rs::BIG5.encode("繁體中文測試資料\n").0);
        fs::write(fixture.path().join("header-big5.txt"), encoded).expect("Big5 source");
        let root = Arc::new(FileAccess::new(
            Arc::new(RepositoryRoot::open(fixture.path()).expect("root")),
            ReadScope::Normal,
        ));
        let mut query = request("繁體");
        query.glob = None;
        query.fixed_strings = Some(true);
        query.fallback_encoding = Some("big5".to_owned());

        let output = execute(&root, &query, 1, &CancellationToken::new())
            .expect("fallback search after ASCII header");
        assert!(
            output.contains("header-big5.txt"),
            "expected hit, got {output}"
        );
    }

    /// One searcher serves every candidate a worker handles. A small text file disarms
    /// binary detection for its own slice search, and a binary file that follows must
    /// still be reported as binary rather than searched as text.
    #[test]
    fn a_binary_file_after_a_text_file_is_still_reported_as_binary() {
        let fixture = tempfile::tempdir().expect("fixture");
        fs::write(fixture.path().join("a_small.rs"), "needle here\n").expect("text source");
        fs::write(
            fixture.path().join("b_large.rs"),
            b"\x00\x01needle\x02".repeat(8_000),
        )
        .expect("binary source");
        let root = Arc::new(FileAccess::new(
            Arc::new(RepositoryRoot::open(fixture.path()).expect("root")),
            ReadScope::Normal,
        ));
        let cancellation = CancellationToken::new();
        let mut query = request("needle");
        query.fixed_strings = Some(true);

        let output = execute(&root, &query, 1, &cancellation).expect("mixed search");

        assert!(output.contains("a_small.rs:1:needle here"));
        assert!(
            output.contains("b_large.rs — binary"),
            "the binary file must be reported, got {output}"
        );
        assert!(
            !output.contains("b_large.rs:"),
            "the binary file must not produce match lines, got {output}"
        );
    }

    #[test]
    fn encoding_arguments_are_rejected_for_the_wrong_target_kind() {
        let (_fixture, root) = fixture();
        let cancellation = CancellationToken::new();

        let mut directory = request("needle");
        directory.encoding = Some("big5".to_owned());
        assert!(matches!(
            execute(&root, &directory, 1, &cancellation),
            Err(GrepError::Validation(_))
        ));

        let mut single = request("needle");
        single.path = Some("src/a.rs".to_owned());
        single.glob = None;
        single.fallback_encoding = Some("big5".to_owned());
        assert!(matches!(
            execute(&root, &single, 1, &cancellation),
            Err(GrepError::Validation(_))
        ));

        let mut unknown = request("needle");
        unknown.fallback_encoding = Some("not-an-encoding".to_owned());
        assert!(matches!(
            execute(&root, &unknown, 1, &cancellation),
            Err(GrepError::Validation(_))
        ));
    }

    #[test]
    fn an_empty_gitignore_filtered_search_recommends_the_retry_flag() {
        let fixture = tempfile::tempdir().expect("fixture");
        fs::write(fixture.path().join(".gitignore"), "hidden.rs\n").expect("ignore file");
        fs::write(fixture.path().join("hidden.rs"), "needle").expect("hidden source");
        let root = Arc::new(FileAccess::new(
            Arc::new(RepositoryRoot::open(fixture.path()).expect("root")),
            ReadScope::Normal,
        ));
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

    fn result_lines(output: &str) -> Vec<&str> {
        output
            .lines()
            .filter(|line| {
                !line.starts_with("Partial:")
                    && *line != "Complete."
                    && !line.starts_with("Skipped")
            })
            .collect()
    }

    #[test]
    fn content_fixed_case_context_and_worker_counts_are_deterministic() {
        let (_fixture, root) = fixture();
        let mut query = request("needle");
        query.fixed_strings = Some(true);
        query.case = Some(CaseMode::Insensitive);
        query.context_lines = Some(1);
        let cancellation = CancellationToken::new();
        let baseline = execute(&root, &query, 1, &cancellation).expect("grep");
        assert!(baseline.contains("src/a.rs"));
        assert!(baseline.contains("-1-before"));
        assert!(baseline.contains("ignored.rs"));
        let mut baseline_lines = result_lines(&baseline)
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        baseline_lines.sort_unstable();
        for workers in [2, 4, 8, 16] {
            let output = execute(&root, &query, workers, &cancellation).expect("parallel grep");
            let mut lines = result_lines(&output)
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            lines.sort_unstable();
            assert_eq!(lines, baseline_lines);
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
        let serial_output = execute(&root, &query, 1, &cancellation).expect("serial grep");
        let parallel_output = execute(&root, &query, 4, &cancellation).expect("parallel grep");
        let mut serial = result_lines(&serial_output)
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let mut parallel = result_lines(&parallel_output)
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        serial.sort_unstable();
        parallel.sort_unstable();
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

        assert!(result_lines(&output).len() <= 2);
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
        let root = Arc::new(FileAccess::new(
            Arc::new(RepositoryRoot::open(fixture.path()).expect("root")),
            ReadScope::Normal,
        ));
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
        let mut page = Page::new(&query, crate::traversal::TraversalSummary::default(), false);
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
        let output = render_with_budget(&query, &page, &cancellation, &burst_512)
            .expect("512-token grep page");
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
        assert!(output.fits_call_budget(&crate::output::TestCallBudget::default(), &cancellation));
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

        let mut expected_lines = result_lines(&expected)
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let mut profile_lines = result_lines(&profile.output)
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        expected_lines.sort_unstable();
        profile_lines.sort_unstable();
        assert_eq!(profile_lines, expected_lines);
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
        let partial = execute_profiled(&root, &partial_query, 4, &cancellation)
            .expect("profiled partial grep");
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

        let mut serial = result_lines(&serial)
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let mut parallel = result_lines(&parallel)
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        serial.sort_unstable();
        parallel.sort_unstable();
        assert_eq!(parallel, serial);
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
        let expected =
            execute_with_traversal(&root, &query, 4, &cancellation, GrepTraversal::Serial)
                .expect("serial grep");
        let mut expected = result_lines(&expected)
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        expected.sort_unstable();

        for traversal in [
            GrepTraversal::SerialLiteralPrefix,
            GrepTraversal::ParallelBatchedLiteralPrefix,
        ] {
            let output = execute_with_traversal(&root, &query, 4, &cancellation, traversal)
                .expect("literal prefix grep");
            let mut lines = result_lines(&output)
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            lines.sort_unstable();
            assert_eq!(lines, expected);
        }
    }

    #[test]
    fn candidate_memory_limit_rejects_the_first_byte_over_the_limit() {
        let (_fixture, root) = fixture();
        let path = root.resolve(Path::new("src/a.rs")).expect("candidate path");
        let candidate = candidate(path).expect("candidate");
        let mut oracle = CandidateCollection::new(
            CandidatePolicy::SoftTarget,
            GrepMemoryPolicy::candidate_only(usize::MAX),
            None,
        );
        oracle.admit(candidate.clone()).expect("oracle admission");
        let limit = oracle.estimated_retained_bytes - 1;
        let mut limited = CandidateCollection::new(
            CandidatePolicy::SoftTarget,
            GrepMemoryPolicy::candidate_only(limit),
            None,
        );

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
            CandidatePolicy::SoftTarget,
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
    fn candidate_set_holds_its_memory_reservation_until_drop() {
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
        let mut collection =
            CandidateCollection::new(CandidatePolicy::SoftTarget, policy, Some(reservation));
        collection.admit(candidate).expect("candidate admission");
        let available_while_collecting = resources.available_memory_bytes();
        let candidates = collection.into_set(TraversalSummary::default(), false);

        assert_eq!(
            resources.available_memory_bytes(),
            available_while_collecting,
            "turning the collection into a searched set must retain the lease"
        );
        drop(candidates);
        assert_eq!(resources.available_memory_bytes(), config.memory_bytes);
    }

    #[test]
    fn grep_without_glob_avoids_path_conversion_and_remains_deterministic() {
        let (_fixture, root) = fixture();
        let mut query = request("needle");
        query.glob = None;
        query.fixed_strings = Some(true);
        let cancellation = CancellationToken::new();
        let baseline = execute(&root, &query, 1, &cancellation).expect("grep without glob");
        assert!(baseline.contains("src/a.rs"));
        assert!(baseline.contains("src/b.rs"));
        assert!(baseline.contains("ignored.rs"));
        let mut baseline_lines = result_lines(&baseline)
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        baseline_lines.sort_unstable();
        for workers in [2, 4, 8, 16] {
            let output = execute(&root, &query, workers, &cancellation).expect("parallel grep");
            let mut lines = result_lines(&output)
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            lines.sort_unstable();
            assert_eq!(lines, baseline_lines);
        }
    }

    #[test]
    fn files_count_and_pagination_modes() {
        let (_fixture, root) = fixture();
        let cancellation = CancellationToken::new();
        let mut files = request("needle");
        files.mode = Some(GrepMode::Files);
        files.limit = Some(1);
        let output = execute(&root, &files, 4, &cancellation).expect("files");
        assert!(
            output.contains("ignored.rs")
                || output.contains("src/a.rs")
                || output.contains("src/b.rs")
        );
        assert!(output.contains("next_offset=1"));

        let mut count = request("needle");
        count.mode = Some(GrepMode::Count);
        let output = execute(&root, &count, 4, &cancellation).expect("count");
        assert!(output.contains("src/a.rs:2"));
        assert!(output.contains("src/b.rs:2"));
        assert!(output.contains("ignored.rs:1"));
    }

    #[test]
    fn include_ignored_false_restores_gitignore_filtering() {
        let (_fixture, root) = fixture();
        let mut query = request("needle");
        query.fixed_strings = Some(true);
        query.include_ignored = Some(false);
        let output = execute(&root, &query, 1, &CancellationToken::new()).expect("respected grep");
        assert!(output.contains("src/a.rs"));
        assert!(!output.contains("ignored.rs"));
    }

    #[test]
    fn directory_grep_skips_denied_trees_while_single_file_grep_still_reads_them() {
        let fixture = tempfile::tempdir().expect("fixture");
        fs::create_dir_all(fixture.path().join("src")).expect("src");
        fs::create_dir_all(fixture.path().join("node_modules/pkg")).expect("node_modules");
        fs::write(fixture.path().join("src/lib.rs"), "needle\n").expect("source");
        fs::write(fixture.path().join("node_modules/pkg/index.rs"), "needle\n").expect("pkg");
        let root = Arc::new(FileAccess::new(
            Arc::new(RepositoryRoot::open(fixture.path()).expect("root")),
            ReadScope::Normal,
        ));
        let mut directory = request("needle");
        directory.fixed_strings = Some(true);
        let output =
            execute(&root, &directory, 1, &CancellationToken::new()).expect("directory grep");
        assert!(output.contains("src/lib.rs"));
        assert!(!output.contains("node_modules"));

        let mut denied_root = request("needle");
        denied_root.path = Some("node_modules".to_owned());
        denied_root.fixed_strings = Some(true);
        assert!(matches!(
            execute(&root, &denied_root, 1, &CancellationToken::new()),
            Err(GrepError::Traversal(
                crate::traversal::TraversalError::DeniedDirectory
            ))
        ));

        let mut single_file = request("needle");
        single_file.path = Some("node_modules/pkg/index.rs".to_owned());
        single_file.fixed_strings = Some(true);
        single_file.glob = None;
        let single = execute(&root, &single_file, 1, &CancellationToken::new())
            .expect("single-file grep inside denied directory");
        assert!(single.contains("node_modules"));
        assert!(single.contains("needle"));
    }

    #[test]
    fn content_and_files_pages_match_the_complete_sequence_across_lanes() {
        let fixture = tempfile::tempdir().expect("fixture");
        fs::create_dir(fixture.path().join("src")).expect("src");
        for index in 0..12 {
            fs::write(
                fixture.path().join(format!("src/{index:02}.rs")),
                format!("before\nneedle needle {index}\nafter\n"),
            )
            .expect("fixture file");
        }
        let root = Arc::new(FileAccess::new(
            Arc::new(RepositoryRoot::open(fixture.path()).expect("root")),
            ReadScope::Normal,
        ));

        for mode in [GrepMode::Content, GrepMode::Files] {
            let mut complete = request("needle");
            complete.fixed_strings = Some(true);
            complete.mode = Some(mode);
            complete.context_lines = (mode == GrepMode::Content).then_some(1);
            complete.limit = Some(1_000);
            let complete_output =
                execute(&root, &complete, 1, &CancellationToken::new()).expect("complete sequence");
            let mut expected = result_lines(&complete_output)
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>();
            expected.sort_unstable();
            assert!(!complete_output.contains("Partial:"));

            for lanes in [1, 4, 16] {
                let output = execute(&root, &complete, lanes, &CancellationToken::new())
                    .expect("parallel sequence");
                let mut actual = result_lines(&output)
                    .into_iter()
                    .map(str::to_owned)
                    .collect::<Vec<_>>();
                actual.sort_unstable();
                assert_eq!(actual, expected, "mode={mode:?} lanes={lanes}");
            }
        }
    }

    #[test]
    fn single_file_modes_scan_through_a_trailing_binary_marker() {
        let (fixture, root) = fixture();
        fs::write(
            fixture.path().join("src/single.rs"),
            b"needle at the start\nordinary\n\0binary tail",
        )
        .expect("binary fixture");

        for mode in [GrepMode::Content, GrepMode::Files, GrepMode::Count] {
            let mut query = request("needle");
            query.path = Some("src/single.rs".to_owned());
            query.glob = None;
            query.fixed_strings = Some(true);
            query.mode = Some(mode);
            query.limit = Some(1);

            assert!(matches!(
                execute(&root, &query, 4, &CancellationToken::new()),
                Err(GrepError::Unsearchable(SkipReason::Binary))
            ));
        }
    }

    #[test]
    fn partial_skip_summary_only_counts_the_searched_prefix() {
        let fixture = tempfile::tempdir().expect("fixture");
        fs::create_dir(fixture.path().join("src")).expect("src");
        fs::write(fixture.path().join("src/00-binary.rs"), b"needle\0tail").expect("binary prefix");
        for index in 1..=4 {
            fs::write(
                fixture.path().join(format!("src/{index:02}.rs")),
                "needle\n",
            )
            .expect("matching fixture");
        }
        fs::write(fixture.path().join("src/99-binary.rs"), b"needle\0tail").expect("binary suffix");
        let root = Arc::new(FileAccess::new(
            Arc::new(RepositoryRoot::open(fixture.path()).expect("root")),
            ReadScope::Normal,
        ));

        let mut complete = request("needle");
        complete.fixed_strings = Some(true);
        let complete_output =
            execute(&root, &complete, 1, &CancellationToken::new()).expect("complete grep");
        assert!(complete_output.contains("Skipped: 2 files"));
        assert!(complete_output.contains(" — binary"));

        let mut query = request("needle");
        query.fixed_strings = Some(true);
        query.limit = Some(1);
        let output = execute(&root, &query, 1, &CancellationToken::new()).expect("partial grep");

        assert!(output.ends_with("Partial: next_offset=1."));
        assert!(!output.contains("Skipped: 2 files"));
        if output.contains("Skipped while producing this page") {
            assert!(output.contains(" — binary"));
        }
    }

    #[test]
    fn files_mode_output_matches_cli_shape_without_pattern_header() {
        let (_fixture, root) = fixture();
        let cancellation = CancellationToken::new();
        let mut files = request("needle");
        files.mode = Some(GrepMode::Files);
        files.fixed_strings = Some(true);
        let output = execute(&root, &files, 1, &cancellation).expect("files");
        let candidates = ["ignored.rs", "src/a.rs", "src/b.rs"]
            .into_iter()
            .map(|p| {
                crate::path::display_path(
                    root.resolve(Path::new(p))
                        .expect("candidate path")
                        .absolute(),
                )
            })
            .collect::<Vec<_>>();
        assert!(!output.contains("Pattern:"));
        assert!(
            candidates
                .iter()
                .any(|candidate| output.starts_with(candidate))
        );
        assert!(
            candidates
                .iter()
                .all(|candidate| output.contains(candidate))
        );
        assert!(!output.contains("Partial:"));
    }

    #[test]
    fn invalid_regex_lookaround_backreference_binary_and_utf16() {
        let (fixture, root) = fixture();
        let cancellation = CancellationToken::new();
        for pattern in ["(", "(?=needle)", r"(needle)\1"] {
            assert!(matches!(
                execute(&root, &request(pattern), 1, &cancellation),
                Err(GrepError::Regex(_))
            ));
        }
        fs::write(fixture.path().join("src/binary.rs"), b"needle\0needle").expect("binary");
        let mut utf16 = vec![0xFF, 0xFE];
        for unit in "needle\n".encode_utf16() {
            utf16.extend(unit.to_le_bytes());
        }
        fs::write(fixture.path().join("src/utf16.rs"), utf16).expect("utf16");
        let output = execute(&root, &request("needle"), 2, &cancellation).expect("grep");
        assert!(output.contains("utf16.rs"));
        assert!(output.contains(" — binary"));
        assert!(output.contains("Skipped: 1 files."));
    }

    #[test]
    fn long_line_is_bounded_and_large_file_remains_searchable() {
        let (fixture, root) = fixture();
        let mut long = String::from("needle");
        long.push_str(&"x".repeat(2 * 1024 * 1024));
        fs::write(fixture.path().join("src/long.rs"), long).expect("long line");
        let mut large = "ordinary line\n".repeat(128);
        large.push_str("needle at the end\n");
        fs::write(fixture.path().join("src/large.rs"), large).expect("large file");

        let output =
            execute(&root, &request("needle"), 4, &CancellationToken::new()).expect("bounded grep");
        assert!(output.contains("src/large.rs"));
        assert!(output.contains("src/long.rs"));
    }

    #[test]
    fn oversized_first_result_is_omitted_and_pagination_advances() {
        let (fixture, root) = fixture();
        fs::create_dir(fixture.path().join("paging")).expect("paging directory");
        let mut large = String::from("needle ");
        large.push_str(&"x".repeat(crate::output::MODEL_BYTE_LIMIT * 2));
        large.push('\n');
        fs::write(fixture.path().join("paging/0-large.rs"), large).expect("large match");
        fs::write(fixture.path().join("paging/z-small.rs"), "needle small\n").expect("small match");
        let mut query = request("needle");
        query.path = Some("paging".to_owned());
        query.fixed_strings = Some(true);
        query.limit = Some(1);

        let first = execute(&root, &query, 1, &CancellationToken::new()).expect("first page");
        assert!(first.contains("next_offset=1"));

        query.offset = Some(1);
        let second = execute(&root, &query, 1, &CancellationToken::new()).expect("second page");
        assert!(second.contains("z-small.rs"));
        assert!(second.contains("needle small"));
    }

    #[test]
    fn page_retention_stays_within_its_memory_budget() {
        let query = request("needle");
        let mut page = Page::new(&query, crate::traversal::TraversalSummary::default(), false);
        for index in 0..1_000 {
            page.push_entry(
                format!("{index}:{}", "x".repeat(20_000)),
                Some(format!(
                    "{index}:[line text omitted: exceeds output budget]"
                )),
            );
        }

        assert!(page.charged <= PAGE_MEMORY_BYTES);
        assert_eq!(page.seen_entries, 1_000);
        assert!(!page.retaining);
        assert!(page.lines.iter().any(|line| line.text.contains("omitted")));
        let output = render(&query, &page, &CancellationToken::new()).expect("bounded page");
        assert!(output.contains("Partial:"));
    }

    #[test]
    fn capture_memory_limit_is_reported() {
        let (fixture, root) = fixture();
        let line = format!("needle {}\n", "x".repeat(20_000));
        fs::write(fixture.path().join("src/z-large.rs"), line.repeat(500))
            .expect("large matching fixture");
        let mut query = request("needle");
        query.fixed_strings = Some(true);
        query.limit = Some(1_000);

        let output = execute(&root, &query, 1, &CancellationToken::new()).expect("directory grep");
        assert!(output.contains("src/b.rs"));
        assert!(output.contains(" — matching content exceeds capture budget"));
        assert!(output.contains("Skipped: 1 files."));
    }

    #[test]
    fn deep_offset_capture_overflow_retries_the_exact_window() {
        let fixture = tempfile::tempdir().expect("fixture");
        let line = format!("needle {}\n", "x".repeat(20_000));
        fs::write(fixture.path().join("matches.rs"), line.repeat(500))
            .expect("large matching fixture");
        let root = Arc::new(FileAccess::new(
            Arc::new(RepositoryRoot::open(fixture.path()).expect("root")),
            ReadScope::Normal,
        ));
        let mut query = request("needle");
        query.glob = None;
        query.fixed_strings = Some(true);
        query.offset = Some(499);
        query.limit = Some(1);

        let output =
            execute(&root, &query, 4, &CancellationToken::new()).expect("exact capture retry");
        assert!(
            output.contains("matches.rs:500:"),
            "expected final match, got {output}"
        );
        assert!(
            !output.contains("capture budget"),
            "unexpected skip: {output}"
        );
    }

    #[test]
    fn nine_mib_line_uses_isolated_heap_retry_and_bounded_capture() {
        let fixture = tempfile::tempdir().expect("fixture");
        let mut line = String::from("needle ");
        line.push_str(&"x".repeat(9 * 1024 * 1024));
        line.push('\n');
        fs::write(fixture.path().join("long.rs"), line).expect("long line");
        let root = Arc::new(FileAccess::new(
            Arc::new(RepositoryRoot::open(fixture.path()).expect("root")),
            ReadScope::Normal,
        ));
        let mut query = request("needle");
        query.glob = None;
        query.fixed_strings = Some(true);

        let output = execute(&root, &query, 4, &CancellationToken::new())
            .expect("isolated large-line retry");
        assert!(
            output.contains("long.rs:1:"),
            "expected long-line match, got {output}"
        );
        assert!(
            output.contains("line text omitted"),
            "expected bounded text, got {output}"
        );
        assert!(
            !output.contains("line exceeds search heap"),
            "unexpected skip: {output}"
        );
    }

    #[test]
    fn isolated_heap_retry_searches_32_and_64_mib_lines() {
        let fixture = tempfile::tempdir().expect("fixture");
        let root = Arc::new(FileAccess::new(
            Arc::new(RepositoryRoot::open(fixture.path()).expect("root")),
            ReadScope::Normal,
        ));
        let mut query = request("needle");
        query.glob = None;
        query.mode = Some(GrepMode::Files);
        query.fixed_strings = Some(true);

        for mebibytes in [32, 64] {
            let mut line = String::from("needle ");
            line.push_str(&"x".repeat(mebibytes * 1024 * 1024));
            line.push('\n');
            fs::write(fixture.path().join("long.rs"), line).expect("long line");
            let output = execute(&root, &query, 4, &CancellationToken::new())
                .expect("isolated large-line retry");
            assert!(
                output.contains("long.rs"),
                "expected {mebibytes} MiB line match, got {output}"
            );
        }
    }

    #[test]
    fn eight_mib_envelope_rejects_a_nine_mib_line_without_oom() {
        let fixture = tempfile::tempdir().expect("fixture");
        let mut line = String::from("needle ");
        line.push_str(&"x".repeat(9 * 1024 * 1024));
        line.push('\n');
        fs::write(fixture.path().join("long.rs"), line).expect("long line");
        let root = Arc::new(FileAccess::new(
            Arc::new(RepositoryRoot::open(fixture.path()).expect("root")),
            ReadScope::Normal,
        ));
        let mut query = request("needle");
        query.glob = None;
        query.fixed_strings = Some(true);

        let output = execute_with_memory_budget(
            &root,
            &query,
            4,
            8 * 1024 * 1024,
            &CancellationToken::new(),
        )
        .expect("controlled large-line failure");
        assert!(
            output.contains("line exceeds search heap"),
            "expected skip, got {output}"
        );
    }

    #[test]
    fn single_file_capture_overflow_is_an_explicit_error() {
        let (fixture, root) = fixture();
        let line = format!("needle {}\n", "x".repeat(20_000));
        fs::write(fixture.path().join("src/a-large.rs"), line.repeat(500))
            .expect("large matching fixture");
        let mut query = request("needle");
        query.path = Some("src/a-large.rs".to_owned());
        query.glob = None;
        query.fixed_strings = Some(true);
        query.limit = Some(1_000);

        assert!(matches!(
            execute(&root, &query, 1, &CancellationToken::new()),
            Err(GrepError::Unsearchable(SkipReason::CaptureBudget))
        ));
    }

    #[test]
    fn traversal_cancellation_uses_the_grep_cancellation_error() {
        let (_fixture, root) = fixture();
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        for traversal in [
            GrepTraversal::Adaptive,
            GrepTraversal::Serial,
            GrepTraversal::ParallelBatched,
        ] {
            assert!(matches!(
                execute_with_traversal(&root, &request("needle"), 1, &cancellation, traversal),
                Err(GrepError::Cancelled)
            ));
        }
    }

    #[test]
    fn file_change_race_skips_only_that_file_with_deterministic_summary() {
        let (fixture, root) = fixture();
        let cancellation = CancellationToken::new();
        let query = request("needle");
        let matcher = build_matcher(&query).expect("matcher");
        let plan = SearchPlan {
            memory: GrepMemoryPolicy::new(256 * 1024 * 1024),
            mode: GrepMode::Content,
            context: 0,
            probe: 10,
            skip: 0,
            allow_early_stop: false,
            encoding: None,
            fallback_encoding: None,
        };
        let changed = candidate(root.resolve(Path::new("src/a.rs")).expect("changed path"))
            .expect("changed candidate");
        let stable = candidate(root.resolve(Path::new("src/b.rs")).expect("stable path"))
            .expect("stable candidate");
        let changed_outcome =
            search_file_with_hook(&root, &changed, &matcher, plan, &cancellation, || {
                fs::write(
                    fixture.path().join("src/a.rs"),
                    "replacement without the old match\n",
                )
                .expect("replace during search");
            })
            .expect("changed outcome");
        assert_eq!(changed_outcome.skip, Some(SkipReason::ChangedWhileSearched));
        let stable_outcome =
            search_file(&root, &stable, &matcher, plan, &cancellation).expect("stable outcome");

        let mut page = Page::new(&query, crate::traversal::TraversalSummary::default(), false);
        page.reduce(changed_outcome, GrepMode::Content, false)
            .expect("reduce changed");
        page.reduce(stable_outcome, GrepMode::Content, false)
            .expect("reduce stable");
        page.mark_complete();
        let output = render(&query, &page, &cancellation).expect("render");
        assert!(output.contains("src/b.rs"));
        assert!(!output.contains("replacement without"));
        assert!(output.contains(" — changed while being searched"));
        assert!(output.contains("Skipped: 1 files."));
    }

    #[test]
    fn directory_search_lists_binary_and_changed_reasons_without_dropping_other_hits() {
        let (fixture, root) = fixture();
        fs::write(fixture.path().join("src/binary.rs"), b"needle\0tail").expect("binary");
        let cancellation = CancellationToken::new();
        let query = request("needle");
        let matcher = build_matcher(&query).expect("matcher");
        let plan = SearchPlan {
            memory: GrepMemoryPolicy::new(256 * 1024 * 1024),
            mode: GrepMode::Content,
            context: 0,
            probe: 10,
            skip: 0,
            allow_early_stop: false,
            encoding: None,
            fallback_encoding: None,
        };
        let changed = candidate(root.resolve(Path::new("src/a.rs")).expect("changed path"))
            .expect("changed candidate");
        let binary = candidate(
            root.resolve(Path::new("src/binary.rs"))
                .expect("binary path"),
        )
        .expect("binary candidate");
        let stable = candidate(root.resolve(Path::new("src/b.rs")).expect("stable path"))
            .expect("stable candidate");
        let changed_outcome =
            search_file_with_hook(&root, &changed, &matcher, plan, &cancellation, || {
                fs::write(
                    fixture.path().join("src/a.rs"),
                    "replacement without the old match\n",
                )
                .expect("replace during search");
            })
            .expect("changed outcome");
        let binary_outcome =
            search_file(&root, &binary, &matcher, plan, &cancellation).expect("binary outcome");
        let stable_outcome =
            search_file(&root, &stable, &matcher, plan, &cancellation).expect("stable outcome");
        assert_eq!(changed_outcome.skip, Some(SkipReason::ChangedWhileSearched));
        assert_eq!(binary_outcome.skip, Some(SkipReason::Binary));

        let mut page = Page::new(&query, crate::traversal::TraversalSummary::default(), false);
        page.reduce(changed_outcome, GrepMode::Content, false)
            .expect("reduce changed");
        page.reduce(binary_outcome, GrepMode::Content, false)
            .expect("reduce binary");
        page.reduce(stable_outcome, GrepMode::Content, false)
            .expect("reduce stable");
        page.mark_complete();
        let output = render(&query, &page, &cancellation).expect("render");
        assert!(output.contains("src/b.rs"));
        assert!(output.contains(" — binary"));
        assert!(output.contains(" — changed while being searched"));
        assert!(output.contains("Skipped: 2 files."));
    }

    #[test]
    fn single_file_change_is_an_explicit_changed_error() {
        let (fixture, root) = fixture();
        let cancellation = CancellationToken::new();
        let mut query = request("needle");
        query.path = Some("src/a.rs".to_owned());
        query.glob = None;
        query.fixed_strings = Some(true);
        let matcher = build_matcher(&query).expect("matcher");
        let plan = SearchPlan {
            memory: GrepMemoryPolicy::new(256 * 1024 * 1024),
            mode: GrepMode::Content,
            context: 0,
            probe: 10,
            skip: 0,
            allow_early_stop: false,
            encoding: None,
            fallback_encoding: None,
        };
        let target = candidate(root.resolve(Path::new("src/a.rs")).expect("single path"))
            .expect("single candidate");
        let outcome = search_file_with_hook(&root, &target, &matcher, plan, &cancellation, || {
            fs::write(
                fixture.path().join("src/a.rs"),
                "replacement without the old match\n",
            )
            .expect("replace during search");
        })
        .expect("changed outcome");

        let mut page = Page::new(&query, crate::traversal::TraversalSummary::default(), false);
        assert!(matches!(
            page.reduce(outcome, GrepMode::Content, true),
            Err(GrepError::Unsearchable(SkipReason::ChangedWhileSearched))
        ));
    }

    #[test]
    fn benchmark_source_variants_preserve_output() {
        let (_fixture, root) = fixture();
        let query = request("needle");
        let expected = execute(&root, &query, 2, &CancellationToken::new()).expect("baseline");
        for source in [
            GrepSourcePolicy::CaptureLimit(1),
            GrepSourcePolicy::CaptureLimit(u64::MAX),
            GrepSourcePolicy::Reader,
            GrepSourcePolicy::FileNever,
            GrepSourcePolicy::MmapAlways,
            GrepSourcePolicy::MmapThreshold(1),
            GrepSourcePolicy::MmapThreshold(u64::MAX),
        ] {
            for pathname_reopen in [
                PathnameReopenPolicy::On,
                PathnameReopenPolicy::Off,
                PathnameReopenPolicy::ParentBatch,
            ] {
                let actual = execute_with_variant(
                    &root,
                    &query,
                    2,
                    &CancellationToken::new(),
                    GrepTraversal::Adaptive,
                    GrepBenchmarkVariant {
                        source,
                        pathname_reopen,
                    },
                )
                .expect("benchmark variant");
                assert_eq!(actual, expected);
            }
        }
    }

    #[test]
    fn same_handle_fingerprint_rejects_rename_with_or_without_pathname_reopen() {
        for pathname_reopen in [PathnameReopenPolicy::On, PathnameReopenPolicy::Off] {
            let (fixture, root) = fixture();
            let cancellation = CancellationToken::new();
            let query = request("needle");
            let matcher = build_matcher(&query).expect("matcher");
            let plan = SearchPlan {
                memory: GrepMemoryPolicy::new(256 * 1024 * 1024),
                mode: GrepMode::Content,
                context: 0,
                probe: 10,
                skip: 0,
                allow_early_stop: false,
                encoding: None,
                fallback_encoding: None,
            };
            let original = fixture.path().join("src/a.rs");
            let renamed = fixture.path().join("src/a-renamed.rs");
            let candidate = candidate(root.resolve(Path::new("src/a.rs")).expect("candidate path"))
                .expect("candidate");
            let outcome = search_file_with_variant_hook(
                &root,
                &candidate,
                &matcher,
                plan,
                &cancellation,
                GrepBenchmarkVariant {
                    source: GrepSourcePolicy::Reader,
                    pathname_reopen,
                },
                || fs::rename(&original, &renamed).expect("rename during validation"),
            )
            .expect("rename outcome");
            assert_eq!(
                outcome.skip,
                Some(SkipReason::ChangedWhileSearched),
                "{pathname_reopen:?}"
            );
        }
    }

    #[test]
    fn same_handle_fingerprint_rejects_truncate_and_same_size_rewrite() {
        for replacement in ["needle\n", "xxxxxx\nxxxxxx xxxxxx\nxxxxx\n"] {
            let (fixture, root) = fixture();
            let cancellation = CancellationToken::new();
            let query = request("needle");
            let matcher = build_matcher(&query).expect("matcher");
            let plan = SearchPlan {
                memory: GrepMemoryPolicy::new(256 * 1024 * 1024),
                mode: GrepMode::Content,
                context: 0,
                probe: 10,
                skip: 0,
                allow_early_stop: false,
                encoding: None,
                fallback_encoding: None,
            };
            let path = fixture.path().join("src/a.rs");
            fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .and_then(|file| file.set_modified(std::time::UNIX_EPOCH))
                .expect("set initial modification time");
            let candidate = candidate(root.resolve(Path::new("src/a.rs")).expect("candidate path"))
                .expect("candidate");
            let outcome = search_file_with_variant_hook(
                &root,
                &candidate,
                &matcher,
                plan,
                &cancellation,
                GrepBenchmarkVariant {
                    source: GrepSourcePolicy::Reader,
                    pathname_reopen: PathnameReopenPolicy::Off,
                },
                || fs::write(&path, replacement).expect("rewrite during validation"),
            )
            .expect("rewrite outcome");
            assert_eq!(outcome.skip, Some(SkipReason::ChangedWhileSearched));
        }
    }

    #[test]
    fn pathname_reopen_detects_delete_recreate_identity_change() {
        for pathname_reopen in [PathnameReopenPolicy::On, PathnameReopenPolicy::Off] {
            let (fixture, root) = fixture();
            let cancellation = CancellationToken::new();
            let query = request("needle");
            let matcher = build_matcher(&query).expect("matcher");
            let plan = SearchPlan {
                memory: GrepMemoryPolicy::new(256 * 1024 * 1024),
                mode: GrepMode::Content,
                context: 0,
                probe: 10,
                skip: 0,
                allow_early_stop: false,
                encoding: None,
                fallback_encoding: None,
            };
            let original = fixture.path().join("src/a.rs");
            let candidate = candidate(root.resolve(Path::new("src/a.rs")).expect("candidate path"))
                .expect("candidate");
            let outcome = search_file_with_variant_hook(
                &root,
                &candidate,
                &matcher,
                plan,
                &cancellation,
                GrepBenchmarkVariant {
                    source: GrepSourcePolicy::Reader,
                    pathname_reopen,
                },
                || {
                    fs::remove_file(&original).expect("delete original");
                    fs::write(&original, "recreated without match\n").expect("recreate pathname");
                },
            )
            .expect("delete recreate outcome");
            assert_eq!(
                outcome.skip.is_some(),
                pathname_reopen == PathnameReopenPolicy::On
            );
        }
    }
}
