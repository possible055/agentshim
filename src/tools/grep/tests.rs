#[cfg(test)]
mod tests {
    use std::{fs, path::Path, sync::Arc};

    use tokio_util::sync::CancellationToken;

    use super::{
        CANDIDATE_SOFT_TARGET_BYTES, CandidateCollection, CandidatePolicy, CaseMode,
        GrepBenchmarkVariant,
        GrepError, GrepMode, GrepRequest, GrepSourcePolicy, GrepTraversal, PAGE_MEMORY_BYTES, Page,
        PathnameReopenPolicy, PlanSink, SearchPlan, build_matcher, candidate, execute,
        execute_with_traversal,
        execute_with_variant, prefer_parallel_candidate_collection, render,
        search_file, search_file_with_hook, search_file_with_variant_hook,
    };
    #[cfg(feature = "bench-internals")]
    use super::execute_profiled;
    use crate::{
        path::{FileAccess, ReadScope, RepositoryRoot},
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

    fn display_subpath(path: &str) -> String {
        path.to_owned()
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
        assert!(baseline.contains(&display_subpath("src/a.rs")));
        assert!(baseline.contains("-1-before"));
        assert!(!baseline.contains("ignored.rs"));
        for workers in [2, 4, 8, 16] {
            assert_eq!(
                execute(&root, &query, workers, &cancellation).expect("parallel grep"),
                baseline
            );
        }
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
        let profile =
            execute_profiled(&root, &query, 4, &cancellation).expect("profiled grep");

        assert_eq!(profile.output, expected);
        assert_eq!(profile.timings.candidate_count, 2);
        assert_eq!(profile.timings.lanes, 2);
        let sequential_ns = profile
            .timings
            .setup_ns
            .saturating_add(profile.timings.candidate_traversal_ns)
            .saturating_add(profile.timings.candidate_sort_ns)
            .saturating_add(profile.timings.search_wall_ns)
            .saturating_add(profile.timings.render_ns);
        assert!(profile.timings.total_ns >= sequential_ns);
        assert!(profile.timings.candidate_traversal_ns > 0);
        assert!(profile.timings.search_wall_ns > 0);
    }

    #[test]
    fn serial_and_parallel_candidate_traversal_are_equivalent() {
        let (_fixture, root) = fixture();
        let mut query = request("needle");
        query.fixed_strings = Some(true);
        query.mode = Some(GrepMode::Count);
        let cancellation = CancellationToken::new();
        let serial = execute_with_traversal(
            &root,
            &query,
            4,
            &cancellation,
            GrepTraversal::Serial,
        )
        .expect("serial grep");
        let parallel = execute_with_traversal(
            &root,
            &query,
            4,
            &cancellation,
            GrepTraversal::ParallelBatched,
        )
        .expect("parallel grep");

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
        let expected = execute_with_traversal(
            &root,
            &query,
            4,
            &cancellation,
            GrepTraversal::Serial,
        )
        .expect("serial grep");

        for traversal in [
            GrepTraversal::SerialLiteralPrefix,
            GrepTraversal::ParallelBatchedLiteralPrefix,
        ] {
            assert_eq!(
                execute_with_traversal(&root, &query, 4, &cancellation, traversal)
                    .expect("literal prefix grep"),
                expected
            );
        }
    }

    #[test]
    fn adaptive_candidate_traversal_keeps_small_roots_serial() {
        let (fixture, root) = fixture();
        let base = root.resolve(Path::new(".")).expect("base");

        assert!(!prefer_parallel_candidate_collection(&root, &base));
        for index in 0..8 {
            fs::create_dir(fixture.path().join(format!("root-{index}"))).expect("root entry");
        }
        assert!(prefer_parallel_candidate_collection(&root, &base));
    }

    #[test]
    fn candidate_soft_target_records_crossing_without_failing() {
        let (_fixture, root) = fixture();
        let path = root.resolve(Path::new("src/a.rs")).expect("candidate path");
        let candidate = candidate(path).expect("candidate");
        let mut collection = CandidateCollection::new(CandidatePolicy::SoftTarget);
        collection.estimated_retained_bytes = CANDIDATE_SOFT_TARGET_BYTES;

        collection.admit(candidate).expect("soft target is not fatal");
        assert_eq!(collection.soft_target_crossings, 1);
        assert_eq!(collection.candidates.len(), 1);
    }

    #[test]
    fn benchmark_fatal_candidate_policy_is_isolated_from_production() {
        let (_fixture, root) = fixture();
        let path = root.resolve(Path::new("src/a.rs")).expect("candidate path");
        let candidate = candidate(path).expect("candidate");
        let mut collection = CandidateCollection::new(CandidatePolicy::FatalCeiling);
        collection.estimated_retained_bytes = CANDIDATE_SOFT_TARGET_BYTES;

        let error = collection.admit(candidate).expect_err("fatal benchmark policy");
        assert!(matches!(error, GrepError::CandidateMemory));
    }

    #[test]
    fn grep_without_glob_avoids_path_conversion_and_remains_deterministic() {
        let (_fixture, root) = fixture();
        let mut query = request("needle");
        query.glob = None;
        query.fixed_strings = Some(true);
        let cancellation = CancellationToken::new();
        let baseline = execute(&root, &query, 1, &cancellation).expect("grep without glob");
        assert!(baseline.contains(&display_subpath("src/a.rs")));
        assert!(baseline.contains(&display_subpath("src/b.rs")));
        assert!(!baseline.contains("ignored.rs"));
        for workers in [2, 4, 8, 16] {
            assert_eq!(
                execute(&root, &query, workers, &cancellation).expect("parallel grep"),
                baseline
            );
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
        assert!(output.contains(&display_subpath("src/a.rs")));
        assert!(output.contains("next_offset=1"));

        let mut count = request("needle");
        count.mode = Some(GrepMode::Count);
        let output = execute(&root, &count, 4, &cancellation).expect("count");
        assert!(output.contains(&format!("{}:2", display_subpath("src/a.rs"))));
        assert!(output.contains(&format!("{}:2", display_subpath("src/b.rs"))));
    }

    #[test]
    fn files_mode_output_matches_cli_shape_without_pattern_header() {
        let (_fixture, root) = fixture();
        let cancellation = CancellationToken::new();
        let mut files = request("needle");
        files.mode = Some(GrepMode::Files);
        files.fixed_strings = Some(true);
        let output = execute(&root, &files, 1, &cancellation).expect("files");
        let first = crate::path::display_path(
            root.resolve(Path::new("src/a.rs"))
                .expect("first result")
                .absolute(),
        );
        assert!(!output.contains("Pattern:"));
        assert!(output.starts_with(&first));
        assert!(output.ends_with("Complete."));
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
        assert!(output.contains("Skipped: 1"));
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
        assert!(output.contains(&display_subpath("src/large.rs")));
        assert!(output.contains("Skipped: 1"));
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
        let mut page = Page::new(&query, crate::traversal::TraversalSummary::default());
        for index in 0..1_000 {
            page.push_entry(
                format!("{index}:{}", "x".repeat(20_000)),
                Some(format!(
                    "{index}:[line text omitted: exceeds output budget]"
                )),
            );
        }

        assert!(page.charged <= PAGE_MEMORY_BYTES);
        assert_eq!(page.total, 1_000);
        assert!(!page.retaining);
        assert!(page.lines.iter().any(|line| line.text.contains("omitted")));
        let output = render(&query, &page, &CancellationToken::new()).expect("bounded page");
        assert!(output.contains("Partial:"));
    }

    #[test]
    fn non_content_sinks_do_not_preallocate_capture_records() {
        let query = request("needle");
        let matcher = build_matcher(&query).expect("matcher");
        let cancellation = CancellationToken::new();
        for mode in [GrepMode::Files, GrepMode::Count] {
            let sink = PlanSink::new(
                &matcher,
                SearchPlan {
                    mode,
                    context: 0,
                    capture_records: 1_000,
                },
                &cancellation,
            );
            assert_eq!(sink.capture_capacity(), 0);
        }
    }

    #[test]
    fn capture_memory_limit_is_reported() {
        let (fixture, root) = fixture();
        let line = format!("needle {}\n", "x".repeat(2_000));
        fs::write(fixture.path().join("src/a-large.rs"), line.repeat(600))
            .expect("large matching fixture");
        let mut query = request("needle");
        query.fixed_strings = Some(true);
        query.limit = Some(1_000);

        assert!(matches!(
            execute(&root, &query, 1, &CancellationToken::new()),
            Err(GrepError::CaptureMemory)
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
                execute_with_traversal(
                    &root,
                    &request("needle"),
                    1,
                    &cancellation,
                    traversal
                ),
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
            mode: GrepMode::Content,
            context: 0,
            capture_records: 10,
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
        assert!(changed_outcome.skipped);
        let stable_outcome =
            search_file(&root, &stable, &matcher, plan, &cancellation).expect("stable outcome");

        let mut page = Page::new(&query, crate::traversal::TraversalSummary::default());
        page.reduce(changed_outcome, GrepMode::Content, false)
            .expect("reduce changed");
        page.reduce(stable_outcome, GrepMode::Content, false)
            .expect("reduce stable");
        let output = render(&query, &page, &cancellation).expect("render");
        assert!(output.contains(&display_subpath("src/b.rs")));
        assert!(!output.contains("replacement without"));
        assert!(output.contains("Skipped: 1 files or entries."));
    }

    #[test]
    fn benchmark_source_variants_preserve_output() {
        let (_fixture, root) = fixture();
        let query = request("needle");
        let expected = execute(&root, &query, 2, &CancellationToken::new()).expect("baseline");
        for source in [
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
    fn production_default_uses_open_time_handle_semantics() {
        assert_eq!(
            GrepBenchmarkVariant::default().pathname_reopen,
            PathnameReopenPolicy::Off
        );
    }

    #[test]
    fn same_handle_fingerprint_rejects_rename_with_or_without_pathname_reopen() {
        for pathname_reopen in [PathnameReopenPolicy::On, PathnameReopenPolicy::Off] {
            let (fixture, root) = fixture();
            let cancellation = CancellationToken::new();
            let query = request("needle");
            let matcher = build_matcher(&query).expect("matcher");
            let plan = SearchPlan {
                mode: GrepMode::Content,
                context: 0,
                capture_records: 10,
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
            assert!(outcome.skipped, "{pathname_reopen:?}");
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
                mode: GrepMode::Content,
                context: 0,
                capture_records: 10,
            };
            let path = fixture.path().join("src/a.rs");
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
            assert!(outcome.skipped);
        }
    }

    #[test]
    fn same_handle_fingerprint_rejects_replace_with_or_without_pathname_reopen() {
        for pathname_reopen in [PathnameReopenPolicy::On, PathnameReopenPolicy::Off] {
            let (fixture, root) = fixture();
            let cancellation = CancellationToken::new();
            let query = request("needle");
            let matcher = build_matcher(&query).expect("matcher");
            let plan = SearchPlan {
                mode: GrepMode::Content,
                context: 0,
                capture_records: 10,
            };
            let original = fixture.path().join("src/a.rs");
            let displaced = fixture.path().join("src/a-old.rs");
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
                    fs::rename(&original, &displaced).expect("displace original");
                    fs::write(&original, "replacement without match\n").expect("replace pathname");
                },
            )
            .expect("replace outcome");
            assert!(outcome.skipped);
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
                mode: GrepMode::Content,
                context: 0,
                capture_records: 10,
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
                outcome.skipped,
                pathname_reopen == PathnameReopenPolicy::On
            );
        }
    }

}
