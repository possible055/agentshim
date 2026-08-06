#[cfg(test)]
mod tests {
    use std::{fs, path::Path, sync::Arc};

    use tokio_util::sync::CancellationToken;

    use super::{
        CaseMode, GrepError, GrepMode, GrepRequest, PAGE_MEMORY_BYTES, Page,
        SearchPlan, build_matcher, candidate, execute, memory_charge, render, search_file,
        search_file_with_hook,
    };
    use crate::{
        path::{FileAccess, ReadScope, RepositoryRoot},
        runtime::MEMORY_BUDGET_BYTES,
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

    fn native_path(path: &str) -> String {
        path.replace('/', std::path::MAIN_SEPARATOR_STR)
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
        assert!(baseline.contains(&native_path("src/a.rs")));
        assert!(baseline.contains("-1-before"));
        assert!(!baseline.contains("ignored.rs"));
        for workers in [2, 4, 8, 16] {
            assert_eq!(
                execute(&root, &query, workers, &cancellation).expect("parallel grep"),
                baseline
            );
        }
    }

    #[test]
    fn grep_without_glob_avoids_path_conversion_and_remains_deterministic() {
        let (_fixture, root) = fixture();
        let mut query = request("needle");
        query.glob = None;
        query.fixed_strings = Some(true);
        let cancellation = CancellationToken::new();
        let baseline = execute(&root, &query, 1, &cancellation).expect("grep without glob");
        assert!(baseline.contains(&native_path("src/a.rs")));
        assert!(baseline.contains(&native_path("src/b.rs")));
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
        assert!(output.contains(&native_path("src/a.rs")));
        assert!(output.contains("next_offset=1"));

        let mut count = request("needle");
        count.mode = Some(GrepMode::Count);
        let output = execute(&root, &count, 4, &cancellation).expect("count");
        assert!(output.contains(&format!("{}:2", native_path("src/a.rs"))));
        assert!(output.contains(&format!("{}:2", native_path("src/b.rs"))));
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
        assert!(output.contains(&native_path("src/large.rs")));
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
    fn runtime_memory_charge_is_conservative_and_bounded() {
        assert!(memory_charge(1) > 16 * 1024 * 1024);
        assert!(memory_charge(16) > memory_charge(1));
        assert!(memory_charge(16) <= MEMORY_BUDGET_BYTES);
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
        assert!(matches!(
            execute(&root, &request("needle"), 1, &cancellation),
            Err(GrepError::Cancelled)
        ));
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
        assert!(output.contains(&native_path("src/b.rs")));
        assert!(!output.contains("replacement without"));
        assert!(output.contains("Skipped: 1 files or entries."));
    }
}
