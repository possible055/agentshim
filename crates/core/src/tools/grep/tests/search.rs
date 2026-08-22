use super::*;

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
    let plan = content_plan();
    let changed = candidate(root.resolve(Path::new("src/a.rs")).expect("changed path"))
        .expect("changed candidate");
    let stable = candidate(root.resolve(Path::new("src/b.rs")).expect("stable path"))
        .expect("stable candidate");
    let changed_outcome = search_file_with(
        &root,
        &changed,
        &matcher,
        plan,
        &cancellation,
        GrepBenchmarkVariant::default(),
        || {
            fs::write(
                fixture.path().join("src/a.rs"),
                "replacement without the old match\n",
            )
            .expect("replace during search");
        },
    )
    .expect("changed outcome");
    assert_eq!(changed_outcome.skip, Some(SkipReason::ChangedWhileSearched));
    let stable_outcome = search_file_with(
        &root,
        &stable,
        &matcher,
        plan,
        &cancellation,
        GrepBenchmarkVariant::default(),
        || {},
    )
    .expect("stable outcome");

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
    let plan = content_plan();
    let changed = candidate(root.resolve(Path::new("src/a.rs")).expect("changed path"))
        .expect("changed candidate");
    let binary = candidate(
        root.resolve(Path::new("src/binary.rs"))
            .expect("binary path"),
    )
    .expect("binary candidate");
    let stable = candidate(root.resolve(Path::new("src/b.rs")).expect("stable path"))
        .expect("stable candidate");
    let changed_outcome = search_file_with(
        &root,
        &changed,
        &matcher,
        plan,
        &cancellation,
        GrepBenchmarkVariant::default(),
        || {
            fs::write(
                fixture.path().join("src/a.rs"),
                "replacement without the old match\n",
            )
            .expect("replace during search");
        },
    )
    .expect("changed outcome");
    let binary_outcome = search_file_with(
        &root,
        &binary,
        &matcher,
        plan,
        &cancellation,
        GrepBenchmarkVariant::default(),
        || {},
    )
    .expect("binary outcome");
    let stable_outcome = search_file_with(
        &root,
        &stable,
        &matcher,
        plan,
        &cancellation,
        GrepBenchmarkVariant::default(),
        || {},
    )
    .expect("stable outcome");
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
    let plan = content_plan();
    let target = candidate(root.resolve(Path::new("src/a.rs")).expect("single path"))
        .expect("single candidate");
    let outcome = search_file_with(
        &root,
        &target,
        &matcher,
        plan,
        &cancellation,
        GrepBenchmarkVariant::default(),
        || {
            fs::write(
                fixture.path().join("src/a.rs"),
                "replacement without the old match\n",
            )
            .expect("replace during search");
        },
    )
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
        let plan = content_plan();
        let original = fixture.path().join("src/a.rs");
        let renamed = fixture.path().join("src/a-renamed.rs");
        let candidate = candidate(root.resolve(Path::new("src/a.rs")).expect("candidate path"))
            .expect("candidate");
        let outcome = search_file_with(
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
        let plan = content_plan();
        let path = fixture.path().join("src/a.rs");
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .and_then(|file| file.set_modified(std::time::UNIX_EPOCH))
            .expect("set initial modification time");
        let candidate = candidate(root.resolve(Path::new("src/a.rs")).expect("candidate path"))
            .expect("candidate");
        let outcome = search_file_with(
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
        let plan = content_plan();
        let original = fixture.path().join("src/a.rs");
        let candidate = candidate(root.resolve(Path::new("src/a.rs")).expect("candidate path"))
            .expect("candidate");
        let outcome = search_file_with(
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
