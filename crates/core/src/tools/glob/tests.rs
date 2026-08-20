#[cfg(test)]
mod tests {
    use std::{fs, path::Path, sync::Arc};

    use tokio_util::sync::CancellationToken;

    #[cfg(feature = "bench-internals")]
    use crate::tools::glob::execute_profiled_with_traversal;
    use crate::tools::glob::{
        BoundedCollector, GlobEntryType, GlobError, GlobMatch, GlobRequest, GlobTraversal,
        MAX_MATCHES, PATH_OMISSION, execute, execute_with_traversal, render, render_with_budget,
    };
    use crate::{
        path::{FileAccess, ReadScope, RepositoryRoot},
        runtime::{
            DEFAULT_GLOB_MEMORY_BYTES, MIN_TOOL_MEMORY_BYTES, MemoryReservation, RuntimeConfig,
            RuntimeResources,
        },
        traversal::TraversalSummary,
    };

    const TEST_LANES: usize = 4;

    fn access(path: &Path) -> Arc<FileAccess> {
        Arc::new(FileAccess::new(
            Arc::new(RepositoryRoot::open(path).expect("root")),
            ReadScope::Normal,
        ))
    }

    fn request(pattern: &str) -> GlobRequest {
        GlobRequest {
            pattern: pattern.to_owned(),
            path: None,
            include_ignored: None,
            entry_type: None,
            offset: None,
            limit: None,
        }
    }

    fn result_lines(output: &str) -> Vec<&str> {
        output
            .lines()
            .filter(|line| {
                !line.starts_with("Partial:")
                    && *line != "Complete."
                    && !line.starts_with("Skipped")
                    && !line.starts_with("Scan stopped:")
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
    fn an_empty_gitignore_filtered_scan_recommends_the_retry_flag() {
        let fixture = tempfile::tempdir().expect("fixture");
        fs::write(fixture.path().join(".gitignore"), "hidden.rs\n").expect("ignore file");
        fs::write(fixture.path().join("hidden.rs"), "source").expect("hidden source");
        let root = access(fixture.path());
        let cancellation = CancellationToken::new();
        let mut query = request("hidden.rs");

        query.include_ignored = Some(false);
        let filtered = execute(&root, &query, TEST_LANES, &cancellation).expect("filtered glob");
        assert!(filtered.contains("No paths matched."));
        assert!(filtered.contains("include_ignored=true"));

        query.include_ignored = Some(true);
        let included = execute(&root, &query, TEST_LANES, &cancellation).expect("included glob");
        assert!(included.contains("hidden.rs"));
        assert!(!included.contains("include_ignored=true"));
    }

    #[test]
    fn empty_results_distinguish_no_matches_from_an_empty_page() {
        let fixture = tempfile::tempdir().expect("fixture");
        let root = access(fixture.path());
        let cancellation = CancellationToken::new();
        let mut query = request("*.missing");

        assert_eq!(
            execute(&root, &query, TEST_LANES, &cancellation).expect("empty glob"),
            "No paths matched."
        );
        query.offset = Some(3);
        assert_eq!(
            execute(&root, &query, TEST_LANES, &cancellation).expect("empty page"),
            "No results at offset=3."
        );
    }

    #[test]
    fn files_are_default_while_directories_and_any_remain_available() {
        let fixture = tempfile::tempdir().expect("fixture");
        fs::create_dir_all(fixture.path().join("src/nested")).expect("directories");
        fs::write(fixture.path().join("src/nested/lib.rs"), "source").expect("source");
        let root = access(fixture.path());
        let directory = crate::path::display_path(
            root.resolve(Path::new("src/nested"))
                .expect("directory")
                .absolute(),
        );
        let file = crate::path::display_path(
            root.resolve(Path::new("src/nested/lib.rs"))
                .expect("file")
                .absolute(),
        );

        let default = execute(
            &root,
            &request("**/*"),
            TEST_LANES,
            &CancellationToken::new(),
        )
        .expect("default glob");
        assert!(default.lines().any(|line| line == file));
        assert!(!default.lines().any(|line| line == directory));

        let mut directories = request("**/*");
        directories.entry_type = Some(GlobEntryType::Directory);
        let directories = execute(&root, &directories, TEST_LANES, &CancellationToken::new())
            .expect("directory glob");
        assert!(directories.lines().any(|line| line == directory));
        assert!(!directories.lines().any(|line| line == file));

        let mut any = request("**/*");
        any.entry_type = Some(GlobEntryType::Any);
        let any = execute(&root, &any, TEST_LANES, &CancellationToken::new()).expect("any glob");
        assert!(any.lines().any(|line| line == directory));
        assert!(any.lines().any(|line| line == file));
    }

    #[test]
    fn ignore_hidden_git_and_pagination_contract() {
        let fixture = tempfile::tempdir().expect("fixture");
        fs::write(fixture.path().join(".gitignore"), "ignored.rs\n").expect("ignore");
        fs::write(fixture.path().join("b.rs"), "b").expect("b");
        fs::write(fixture.path().join("a.rs"), "a").expect("a");
        fs::write(fixture.path().join(".hidden.rs"), "h").expect("hidden");
        fs::write(fixture.path().join("ignored.rs"), "i").expect("ignored");
        fs::create_dir(fixture.path().join(".git")).expect("git");
        fs::write(fixture.path().join(".git/internal.rs"), "g").expect("git file");
        let root = access(fixture.path());
        let mut query = request("*.rs");
        query.limit = Some(2);
        let first = execute(&root, &query, TEST_LANES, &CancellationToken::new()).expect("glob");
        assert!(first.contains(".hidden.rs"));
        assert!(first.contains("a.rs"));
        assert!(first.ends_with("Partial: next_offset=2."));

        query.include_ignored = Some(false);
        query.limit = Some(100);
        let respected =
            execute(&root, &query, TEST_LANES, &CancellationToken::new()).expect("respected glob");
        assert!(!respected.contains("ignored.rs\n"));
        assert!(!respected.contains(".git/internal.rs"));

        query.include_ignored = Some(true);
        let all = execute(&root, &query, TEST_LANES, &CancellationToken::new()).expect("all glob");
        assert!(all.contains("ignored.rs"));
        assert!(!all.contains(".git/internal.rs"));
    }

    #[test]
    fn glob_skips_denied_directories_and_rejects_them_as_roots() {
        let fixture = tempfile::tempdir().expect("fixture");
        fs::create_dir_all(fixture.path().join("src")).expect("src");
        fs::create_dir_all(fixture.path().join("node_modules/pkg")).expect("node_modules");
        fs::create_dir_all(fixture.path().join("target/debug")).expect("target");
        fs::write(fixture.path().join("src/lib.rs"), "source").expect("source");
        fs::write(fixture.path().join("node_modules/pkg/index.rs"), "pkg").expect("pkg");
        fs::write(fixture.path().join("target/debug/out.rs"), "out").expect("out");
        let root = access(fixture.path());
        let output = execute(
            &root,
            &request("**/*.rs"),
            TEST_LANES,
            &CancellationToken::new(),
        )
        .expect("glob");
        assert!(output.contains("src"));
        assert!(!output.contains("node_modules"));
        assert!(!output.contains("target"));

        let mut denied = request("**/*.rs");
        denied.path = Some("node_modules".to_owned());
        assert!(matches!(
            execute(&root, &denied, TEST_LANES, &CancellationToken::new()),
            Err(GlobError::Traversal(
                crate::traversal::TraversalError::DeniedDirectory
            ))
        ));
    }

    #[test]
    fn glob_output_matches_cli_shape_without_pattern_header() {
        let fixture = tempfile::tempdir().expect("fixture");
        fs::write(fixture.path().join("a.rs"), "a").expect("a");
        fs::write(fixture.path().join("b.rs"), "b").expect("b");
        let root = access(fixture.path());
        let mut query = request("*.rs");
        query.limit = Some(100);
        let output = execute(&root, &query, TEST_LANES, &CancellationToken::new()).expect("glob");
        assert!(!output.contains("Pattern:"));
        assert!(
            output.starts_with(&crate::path::display_path(
                root.resolve(std::path::Path::new("a.rs"))
                    .expect("a")
                    .absolute()
            ))
        );
        assert!(!output.contains("Partial:"));
    }

    #[test]
    fn dense_glob_matches_native_paths_without_prebuilt_slash_strings() {
        let fixture = tempfile::tempdir().expect("fixture");
        fs::create_dir_all(fixture.path().join("src/nested")).expect("directories");
        for path in ["top.rs", "src/lib.rs", "src/nested/Unicode 界.rs"] {
            fs::write(fixture.path().join(path), "source").expect("source");
        }
        let root = access(fixture.path());
        let mut query = request("**/*");
        query.limit = Some(100);
        let output =
            execute(&root, &query, TEST_LANES, &CancellationToken::new()).expect("dense glob");
        for path in ["top.rs", "src/lib.rs", "src/nested/Unicode 界.rs"] {
            let absolute = root.resolve(Path::new(path)).expect("resolved path");
            assert!(output.contains(&crate::path::display_path(absolute.absolute())));
        }
    }

    #[test]
    fn parallel_batched_glob_matches_serial_result_set() {
        let fixture = tempfile::tempdir().expect("fixture");
        fs::write(fixture.path().join(".gitignore"), "ignored/**\n").expect("ignore");
        fs::create_dir_all(fixture.path().join("src/deep")).expect("source directories");
        fs::create_dir_all(fixture.path().join("ignored")).expect("ignored directory");
        for index in (0..513).rev() {
            let directory = if index % 2 == 0 { "src" } else { "src/deep" };
            fs::write(
                fixture
                    .path()
                    .join(directory)
                    .join(format!("file-{index:04}.rs")),
                "source",
            )
            .expect("source");
        }
        fs::write(fixture.path().join("ignored/hidden.rs"), "ignored").expect("ignored");
        let root = access(fixture.path());
        let mut query = request("**/*.rs");
        query.limit = Some(1_000);
        let cancellation = CancellationToken::new();
        let serial = execute_with_traversal(
            &root,
            &query,
            TEST_LANES,
            &cancellation,
            GlobTraversal::Serial,
        )
        .expect("serial glob");
        let parallel = execute_with_traversal(
            &root,
            &query,
            TEST_LANES,
            &cancellation,
            GlobTraversal::ParallelBatched,
        )
        .expect("parallel glob");
        assert!(!result_lines(&serial).is_empty());
        assert!(!result_lines(&parallel).is_empty());

        for (pattern, entry_type) in [
            ("**/*.missing", GlobEntryType::File),
            ("**/*", GlobEntryType::Directory),
            ("**/*", GlobEntryType::Any),
        ] {
            let mut query = request(pattern);
            query.entry_type = Some(entry_type);
            query.limit = Some(1_000);
            let serial = execute_with_traversal(
                &root,
                &query,
                TEST_LANES,
                &cancellation,
                GlobTraversal::Serial,
            )
            .expect("serial glob");
            let parallel = execute_with_traversal(
                &root,
                &query,
                TEST_LANES,
                &cancellation,
                GlobTraversal::ParallelBatched,
            )
            .expect("parallel glob");
            assert!(!result_lines(&serial).is_empty());
            assert!(!result_lines(&parallel).is_empty());
        }
    }

    #[test]
    fn literal_prefix_glob_preserves_serial_and_parallel_result_set() {
        let fixture = tempfile::tempdir().expect("fixture");
        fs::write(fixture.path().join(".gitignore"), "src/deep/ignored.rs\n").expect("ignore");
        fs::create_dir_all(fixture.path().join("src/deep")).expect("deep");
        fs::create_dir_all(fixture.path().join("src/sibling")).expect("sibling");
        fs::write(fixture.path().join("src/deep/a.rs"), "source").expect("a");
        fs::write(fixture.path().join("src/deep/ignored.rs"), "ignored").expect("ignored");
        fs::write(fixture.path().join("src/sibling/b.rs"), "source").expect("b");
        let root = access(fixture.path());
        let query = request("src/deep/*.rs");
        let cancellation = CancellationToken::new();
        let expected = execute_with_traversal(
            &root,
            &query,
            TEST_LANES,
            &cancellation,
            GlobTraversal::Serial,
        )
        .expect("serial glob");
        let expected = sorted_result_lines(&expected);

        for traversal in [
            GlobTraversal::SerialLiteralPrefix,
            GlobTraversal::ParallelBatchedLiteralPrefix,
        ] {
            let output =
                execute_with_traversal(&root, &query, TEST_LANES, &cancellation, traversal)
                    .expect("literal prefix glob");
            assert_eq!(sorted_result_lines(&output), expected);
        }
    }

    #[test]
    fn glob_bounded_collector_preserves_result_set() {
        let fixture = tempfile::tempdir().expect("fixture");
        for index in 0..32 {
            fs::write(fixture.path().join(format!("file-{index:02}.rs")), "source")
                .expect("source");
        }
        let root = access(fixture.path());
        let query = request("*.rs");
        let cancellation = CancellationToken::new();
        let serial = execute_with_traversal(
            &root,
            &query,
            TEST_LANES,
            &cancellation,
            GlobTraversal::Serial,
        )
        .expect("serial glob");
        let parallel = execute_with_traversal(
            &root,
            &query,
            TEST_LANES,
            &cancellation,
            GlobTraversal::ParallelBatched,
        )
        .expect("parallel glob");

        assert_eq!(sorted_result_lines(&parallel), sorted_result_lines(&serial));
    }

    #[test]
    fn glob_early_stop_at_limit() {
        let fixture = tempfile::tempdir().expect("fixture");
        for index in 0..32 {
            fs::write(fixture.path().join(format!("file-{index:02}.rs")), "source")
                .expect("source");
        }
        let root = access(fixture.path());
        let mut query = request("*.rs");
        query.limit = Some(2);

        let output =
            execute(&root, &query, TEST_LANES, &CancellationToken::new()).expect("early-stop glob");

        assert!(output.contains("Scan stopped: page limit reached;"));
        assert!(output.contains("Partial: next_offset=2"));
        assert!(result_lines(&output).len() <= 2);
    }

    #[cfg(feature = "bench-internals")]
    #[test]
    fn profiled_parallel_glob_preserves_output_and_records_batches() {
        let fixture = tempfile::tempdir().expect("fixture");
        for index in 0..16 {
            let shard = fixture.path().join(format!("shard-{index}"));
            fs::create_dir(&shard).expect("shard");
            fs::write(shard.join(format!("file-{index}.rs")), "source").expect("source");
        }
        let root = access(fixture.path());
        let query = request("**/*.rs");
        let cancellation = CancellationToken::new();
        let expected = execute_with_traversal(
            &root,
            &query,
            TEST_LANES,
            &cancellation,
            GlobTraversal::Serial,
        )
        .expect("serial glob");
        let profile = execute_profiled_with_traversal(
            &root,
            &query,
            TEST_LANES,
            &cancellation,
            GlobTraversal::ParallelBatched,
        )
        .expect("profiled glob");

        assert_eq!(
            sorted_result_lines(&profile.output),
            sorted_result_lines(&expected)
        );
        assert!(profile.timings.total_ns >= profile.timings.traversal_wall_ns);
        assert!(profile.timings.batches > 0);
        assert_eq!(profile.timings.matched_entries, 16);
    }

    #[test]
    fn bounded_collector_memory_limit_rejects_the_first_byte_over_the_limit() {
        let fixture = tempfile::tempdir().expect("fixture");
        let root = RepositoryRoot::open(fixture.path()).expect("root");
        let path = root.resolve(Path::new("candidate.rs")).expect("candidate");
        let mut oracle = BoundedCollector::new(1, usize::MAX, None).expect("oracle");
        oracle.admit(&path).expect("oracle admission");
        let limit = oracle.retained_memory_bytes() - 1;
        let mut limited = BoundedCollector::new(1, limit, None).expect("limited top-k");

        assert!(matches!(limited.admit(&path), Err(GlobError::Memory)));
    }

    #[test]
    fn bounded_collector_global_memory_pressure_is_retryable_and_releases_permits() {
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

        assert!(matches!(
            BoundedCollector::new(128, DEFAULT_GLOB_MEMORY_BYTES, Some(reservation)),
            Err(GlobError::MemoryBusy)
        ));
        drop(pressure);
        assert!(
            resources
                .try_reserve_memory(MIN_TOOL_MEMORY_BYTES)
                .is_some()
        );
    }

    #[test]
    fn invalid_pattern_is_explicit() {
        let fixture = tempfile::tempdir().expect("fixture");
        let root = access(fixture.path());
        assert!(matches!(
            execute(&root, &request("["), TEST_LANES, &CancellationToken::new()),
            Err(GlobError::Pattern(_))
        ));
    }

    #[test]
    fn token_dense_path_is_omitted_and_pagination_advances() {
        let retained = vec![
            GlobMatch {
                absolute: " x".repeat(12_000),
                charge: 0,
            },
            GlobMatch {
                absolute: "second".to_owned(),
                charge: 0,
            },
        ];
        let mut query = request("**/*");
        query.limit = Some(1);

        let first_page = render(
            &query,
            &retained,
            retained.len(),
            &TraversalSummary::default(),
            false,
            &CancellationToken::new(),
        )
        .expect("first page");
        assert!(first_page.contains(PATH_OMISSION));
        assert!(first_page.contains("next_offset=1"));

        query.offset = Some(1);
        let second_page = render(
            &query,
            &retained,
            retained.len(),
            &TraversalSummary::default(),
            false,
            &CancellationToken::new(),
        )
        .expect("second page");
        assert!(second_page.contains("second"));
        assert!(!second_page.contains("Partial:"));
    }

    #[test]
    fn partial_pages_keep_the_shown_offset_under_burst_and_item_ceilings() {
        let fixture = tempfile::tempdir().expect("fixture");
        let root = RepositoryRoot::open(fixture.path()).expect("root");
        let retained = (0..80)
            .map(|index| {
                let path = root
                    .resolve(Path::new(&format!("file-{index:02}.rs")))
                    .expect("path");
                GlobMatch {
                    absolute: format!(
                        "{} {}",
                        crate::path::display_path(path.absolute()),
                        " x".repeat(20)
                    ),
                    charge: 0,
                }
            })
            .collect::<Vec<_>>();
        let query = request("**/*");
        let cancellation = CancellationToken::new();
        let burst_512 = crate::output::TestCallBudget {
            ceiling: 512,
            ..crate::output::TestCallBudget::default()
        };
        let output = render_with_budget(
            &query,
            &retained,
            retained.len(),
            &TraversalSummary::default(),
            false,
            &cancellation,
            &burst_512,
        )
        .expect("512-token glob page");
        let next = output
            .lines()
            .find_map(|line| {
                line.strip_prefix("Partial: next_offset=")?
                    .trim_end_matches('.')
                    .parse::<usize>()
                    .ok()
            })
            .expect("partial cursor");
        let shown = output
            .lines()
            .filter(|line| !line.starts_with("Partial:") && !line.starts_with("Skipped"))
            .count();
        assert_eq!(next, shown);
        assert!(output.fits_call_budget(&burst_512, &cancellation));
        assert!(output.fits_call_budget(&crate::output::TestCallBudget::default(), &cancellation));
    }

    #[test]
    fn match_cap_is_a_scan_stopped_success_page() {
        let fixture = tempfile::tempdir().expect("fixture");
        let root = RepositoryRoot::open(fixture.path()).expect("root");
        let first = root.resolve(Path::new("first")).expect("first path");
        let retained = vec![GlobMatch {
            absolute: crate::path::display_path(first.absolute()),
            charge: 0,
        }];
        let mut query = request("**/*");
        query.limit = Some(1);

        let output = render(
            &query,
            &retained,
            MAX_MATCHES,
            &TraversalSummary::default(),
            true,
            &CancellationToken::new(),
        )
        .expect("scan-stopped page");
        assert!(output.contains("Scan stopped: more than "));
        assert!(output.contains(" paths matched; narrow pattern or path."));
        assert!(output.contains("Partial: next_offset="));
    }
}
