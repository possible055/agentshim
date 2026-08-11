#[cfg(test)]
mod tests {
    use std::{fs, path::Path, sync::Arc};

    use globset::GlobBuilder;
    use tokio_util::sync::CancellationToken;

    use super::{
        GlobEntryType, GlobError, GlobMatch, GlobRequest, GlobTraversal, MAX_MATCHES, PATH_OMISSION,
        TopK, execute, execute_with_traversal, memory_charge, record_match, render,
    };
    #[cfg(feature = "bench-internals")]
    use super::execute_profiled_with_traversal;
    use crate::{
        path::{FileAccess, ReadScope, RepositoryRoot, slash_path},
        runtime::{
            DEFAULT_GLOB_MEMORY_BYTES, MIN_TOOL_MEMORY_BYTES, MemoryReservation, RuntimeConfig,
            RuntimeResources,
        },
        traversal::{TraversalSummary, prefer_parallel_root},
    };

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

        let default = execute(&root, &request("**/*"), &CancellationToken::new())
            .expect("default glob");
        assert!(default.lines().any(|line| line == file));
        assert!(!default.lines().any(|line| line == directory));

        let mut directories = request("**/*");
        directories.entry_type = Some(GlobEntryType::Directory);
        let directories = execute(&root, &directories, &CancellationToken::new())
            .expect("directory glob");
        assert!(directories.lines().any(|line| line == directory));
        assert!(!directories.lines().any(|line| line == file));

        let mut any = request("**/*");
        any.entry_type = Some(GlobEntryType::Any);
        let any = execute(&root, &any, &CancellationToken::new()).expect("any glob");
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
        let first = execute(&root, &query, &CancellationToken::new()).expect("glob");
        assert!(first.contains(".hidden.rs"));
        assert!(first.contains("a.rs"));
        assert!(!first.contains("ignored.rs\n"));
        assert!(first.ends_with("Partial: next_offset=2."));

        query.include_ignored = Some(true);
        query.limit = Some(100);
        let all = execute(&root, &query, &CancellationToken::new()).expect("all glob");
        assert!(all.contains("ignored.rs"));
        assert!(!all.contains(".git/internal.rs"));
    }

    #[test]
    fn glob_output_matches_cli_shape_without_pattern_header() {
        let fixture = tempfile::tempdir().expect("fixture");
        fs::write(fixture.path().join("a.rs"), "a").expect("a");
        fs::write(fixture.path().join("b.rs"), "b").expect("b");
        let root = access(fixture.path());
        let mut query = request("*.rs");
        query.limit = Some(100);
        let output = execute(&root, &query, &CancellationToken::new()).expect("glob");
        assert!(!output.contains("Pattern:"));
        assert!(output.starts_with(&crate::path::display_path(
            root.resolve(std::path::Path::new("a.rs")).expect("a").absolute()
        )));
        assert!(output.ends_with("Complete."));
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
        let output = execute(&root, &query, &CancellationToken::new()).expect("dense glob");
        for path in ["top.rs", "src/lib.rs", "src/nested/Unicode 界.rs"] {
            let absolute = root.resolve(Path::new(path)).expect("resolved path");
            assert!(output.contains(&crate::path::display_path(absolute.absolute())));
        }
    }

    #[test]
    fn parallel_batched_glob_matches_serial_output() {
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
        query.offset = Some(137);
        query.limit = Some(71);
        let cancellation = CancellationToken::new();
        let serial = execute_with_traversal(
            &root,
            &query,
            &cancellation,
            GlobTraversal::Serial,
        )
        .expect("serial glob");
        let parallel = execute_with_traversal(
            &root,
            &query,
            &cancellation,
            GlobTraversal::ParallelBatched,
        )
        .expect("parallel glob");
        assert_eq!(parallel, serial);
    }

    #[test]
    fn literal_prefix_glob_preserves_serial_and_parallel_output() {
        let fixture = tempfile::tempdir().expect("fixture");
        fs::write(
            fixture.path().join(".gitignore"),
            "src/deep/ignored.rs\n",
        )
        .expect("ignore");
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
            &cancellation,
            GlobTraversal::Serial,
        )
        .expect("serial glob");

        for traversal in [
            GlobTraversal::SerialLiteralPrefix,
            GlobTraversal::ParallelBatchedLiteralPrefix,
        ] {
            assert_eq!(
                execute_with_traversal(&root, &query, &cancellation, traversal)
                    .expect("literal prefix glob"),
                expected
            );
        }
    }

    #[test]
    fn adaptive_selector_keeps_small_roots_serial_and_sharded_roots_parallel() {
        let fixture = tempfile::tempdir().expect("fixture");
        let root = access(fixture.path());
        let base = root.resolve(Path::new(".")).expect("base");
        for index in 0..7 {
            fs::create_dir(fixture.path().join(format!("shard-{index}"))).expect("small shard");
        }
        assert!(!prefer_parallel_root(&root, &base));
        fs::create_dir(fixture.path().join("shard-7")).expect("parallel shard");
        assert!(prefer_parallel_root(&root, &base));
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
            &cancellation,
            GlobTraversal::Serial,
        )
        .expect("serial glob");
        let profile = execute_profiled_with_traversal(
            &root,
            &query,
            &cancellation,
            GlobTraversal::ParallelBatched,
        )
        .expect("profiled glob");

        assert_eq!(profile.output, expected);
        assert!(profile.timings.total_ns >= profile.timings.traversal_wall_ns);
        assert!(profile.timings.batches > 0);
        assert_eq!(profile.timings.matched_entries, 16);
    }

    #[test]
    fn native_path_matching_equals_slash_path_matching() {
        let native = Path::new("src").join("nested").join("Unicode 界.rs");
        let slash = slash_path(&native).expect("slash path");
        for pattern in ["**/*.rs", "src/**", "**/*", "*.txt"] {
            let matcher = GlobBuilder::new(pattern)
                .literal_separator(true)
                .backslash_escape(false)
                .build()
                .expect("glob")
                .compile_matcher();
            assert_eq!(
                matcher.is_match(&native),
                matcher.is_match(Path::new(&slash)),
                "pattern {pattern}"
            );
        }
    }

    #[test]
    fn top_k_matches_full_sort_oracle() {
        let fixture = tempfile::tempdir().expect("fixture");
        let root = RepositoryRoot::open(fixture.path()).expect("root");
        let mut paths = Vec::new();
        let mut oracle = Vec::new();
        for index in (0..256).rev() {
            let path = format!("file-{index:06}.rs");
            let resolved = root.resolve(Path::new(&path)).expect("resolve");
            oracle.push(resolved.sort_key().clone());
            paths.push(resolved);
        }
        oracle.sort();
        for (offset, limit) in [(0_usize, 17_usize), (57, 31), (246, 10), (257, 5)] {
            let mut top = TopK::new(
                offset.saturating_add(limit).min(paths.len()),
                DEFAULT_GLOB_MEMORY_BYTES,
                None,
            )
            .expect("top-k");
            for path in &paths {
                top.admit(path).expect("admit");
            }
            let actual = top
                .into_sorted(&CancellationToken::new())
                .expect("sort")
                .into_iter()
                .skip(offset)
                .take(limit)
                .map(|entry| entry.sort_key)
                .collect::<Vec<_>>();
            let expected = oracle
                .iter()
                .skip(offset)
                .take(limit)
                .cloned()
                .collect::<Vec<_>>();
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn top_k_memory_limit_rejects_the_first_byte_over_the_limit() {
        let fixture = tempfile::tempdir().expect("fixture");
        let root = RepositoryRoot::open(fixture.path()).expect("root");
        let path = root.resolve(Path::new("candidate.rs")).expect("candidate");
        let mut oracle = TopK::new(1, usize::MAX, None).expect("oracle");
        oracle.admit(&path).expect("oracle admission");
        let limit = oracle.retained_memory_bytes() - 1;
        let mut limited = TopK::new(1, limit, None).expect("limited top-k");

        assert!(matches!(limited.admit(&path), Err(GlobError::Memory)));
    }

    #[test]
    fn top_k_global_memory_pressure_is_retryable_and_releases_permits() {
        let mut config = RuntimeConfig::for_tests(1);
        config.memory_bytes = MIN_TOOL_MEMORY_BYTES;
        let resources = RuntimeResources::new(config);
        let initial = resources
            .try_reserve_memory(1024)
            .expect("initial reservation");
        let reservation = MemoryReservation::from_initial(resources.clone(), initial, 1024);
        let pressure = resources
            .try_reserve_memory(MIN_TOOL_MEMORY_BYTES - 1024)
            .expect("competing reservation");

        assert!(matches!(
            TopK::new(1, DEFAULT_GLOB_MEMORY_BYTES, Some(reservation)),
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
    fn invalid_pattern_and_match_limit_are_explicit() {
        let fixture = tempfile::tempdir().expect("fixture");
        let root = access(fixture.path());
        assert!(matches!(
            execute(&root, &request("["), &CancellationToken::new()),
            Err(GlobError::Pattern(_))
        ));
        let mut total = MAX_MATCHES;
        assert!(matches!(
            record_match(&mut total),
            Err(GlobError::TooManyMatches)
        ));
    }

    #[test]
    fn token_dense_path_is_omitted_and_pagination_advances() {
        let fixture = tempfile::tempdir().expect("fixture");
        let root = RepositoryRoot::open(fixture.path()).expect("root");
        let first = root.resolve(Path::new("first")).expect("first path");
        let second = root.resolve(Path::new("second")).expect("second path");
        let retained = vec![
            GlobMatch {
                sort_key: first.sort_key().clone(),
                absolute: " x".repeat(12_000),
                charge: 0,
            },
            GlobMatch {
                sort_key: second.sort_key().clone(),
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
            TraversalSummary::default(),
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
            TraversalSummary::default(),
            &CancellationToken::new(),
        )
        .expect("second page");
        assert!(second_page.contains("second"));
        assert!(second_page.ends_with("Complete."));
    }

    #[test]
    fn runtime_memory_charge_includes_safety_margin() {
        let default = request("**/*");
        assert_eq!(
            memory_charge(&default),
            8 * 1024 * 1024 + 200 * std::mem::size_of::<GlobMatch>()
        );
        let mut maximum = default;
        maximum.offset = Some(MAX_MATCHES - 1);
        maximum.limit = Some(1);
        assert_eq!(
            memory_charge(&maximum),
            8 * 1024 * 1024 + MAX_MATCHES * std::mem::size_of::<GlobMatch>()
        );
        assert!(memory_charge(&maximum) <= DEFAULT_GLOB_MEMORY_BYTES);
        assert!(memory_charge(&maximum).saturating_mul(16) < 1024 * 1024 * 1024);
    }
}
