use super::*;

#[test]
fn files_count_and_pagination_modes() {
    let (_fixture, root) = fixture();
    let cancellation = CancellationToken::new();
    let mut files = request("needle");
    files.mode = Some(GrepMode::Files);
    files.limit = Some(1);
    let output = execute(&root, &files, 4, &cancellation).expect("files");
    assert!(
        output.contains("ignored.rs") || output.contains("src/a.rs") || output.contains("src/b.rs")
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
    let root = root_at(fixture.path());
    let mut directory = request("needle");
    directory.fixed_strings = Some(true);
    let output = execute(&root, &directory, 1, &CancellationToken::new()).expect("directory grep");
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
    let root = root_at(fixture.path());

    for mode in [GrepMode::Content, GrepMode::Files] {
        let mut complete = request("needle");
        complete.fixed_strings = Some(true);
        complete.mode = Some(mode);
        complete.context_lines = (mode == GrepMode::Content).then_some(1);
        complete.limit = Some(1_000);
        let complete_output =
            execute(&root, &complete, 1, &CancellationToken::new()).expect("complete sequence");
        let expected = sorted_result_lines(&complete_output);
        assert!(!complete_output.contains("Partial:"));

        for lanes in [1, 4, 16] {
            let output = execute(&root, &complete, lanes, &CancellationToken::new())
                .expect("parallel sequence");
            assert_eq!(
                sorted_result_lines(&output),
                expected,
                "mode={mode:?} lanes={lanes}"
            );
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
    let root = root_at(fixture.path());

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
    assert!(output.contains("Skipped while producing this page"));
    assert!(output.contains(" — binary"));
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
    large.push_str("\nneedle small\n");
    fs::write(fixture.path().join("paging/mixed.rs"), large).expect("mixed matches");
    let mut query = request("needle");
    query.path = Some("paging".to_owned());
    query.fixed_strings = Some(true);
    query.limit = Some(1);

    let first = execute(&root, &query, 1, &CancellationToken::new()).expect("first page");
    assert!(first.contains("mixed.rs:1:"));
    assert!(first.contains("[line text omitted: exceeds output budget]"));
    assert!(!first.contains("needle small"));
    assert!(first.contains("next_offset=1"));

    query.offset = Some(1);
    let second = execute(&root, &query, 1, &CancellationToken::new()).expect("second page");
    assert!(second.contains("mixed.rs:2:"));
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
    fs::write(fixture.path().join("matches.rs"), line.repeat(500)).expect("large matching fixture");
    let root = root_at(fixture.path());
    let mut query = request("needle");
    query.glob = None;
    query.fixed_strings = Some(true);
    query.offset = Some(499);
    query.limit = Some(1);

    let output = execute(&root, &query, 4, &CancellationToken::new()).expect("exact capture retry");
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
    let root = root_at(fixture.path());
    let mut query = request("needle");
    query.glob = None;
    query.fixed_strings = Some(true);

    let output =
        execute(&root, &query, 4, &CancellationToken::new()).expect("isolated large-line retry");
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
    let root = root_at(fixture.path());
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
    let root = root_at(fixture.path());
    let mut query = request("needle");
    query.glob = None;
    query.fixed_strings = Some(true);

    let output =
        execute_with_memory_budget(&root, &query, 4, 8 * 1024 * 1024, &CancellationToken::new())
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
