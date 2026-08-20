use super::*;

#[test]
fn continuation_parameter_combinations_are_rejected_before_io() {
    let mut offset_without_pages = request("document.pdf");
    offset_without_pages.pdf_cursor = Some(sample_cursor(Some(0)));
    assert!(matches!(
        offset_without_pages.validate(),
        Err(ReadError::Validation(_))
    ));

    let mut offset_with_range = request("document.pdf");
    offset_with_range.pdf_cursor = Some(sample_cursor(Some(0)));
    offset_with_range.pages = Some("1-3".to_owned());
    assert!(matches!(
        offset_with_range.validate(),
        Err(ReadError::Validation(_))
    ));

    let mut offset_with_image = request("document.pdf");
    offset_with_image.pdf_cursor = Some(sample_cursor(Some(0)));
    offset_with_image.pages = Some("2".to_owned());
    offset_with_image.pdf_mode = Some(PdfMode::Image);
    assert!(matches!(
        offset_with_image.validate(),
        Err(ReadError::Validation(_))
    ));

    // Token shapes are covered by the cursor module's own tests; this only proves
    // validation surfaces a decode failure rather than reaching the filesystem.
    let mut malformed_cursor = request("document.pdf");
    malformed_cursor.pdf_cursor = Some("512".to_owned());
    assert!(matches!(
        malformed_cursor.validate(),
        Err(ReadError::Validation(_))
    ));

    let mut valid = request("document.pdf");
    valid.pdf_cursor = Some(sample_cursor(Some(512)));
    valid.pages = Some("2".to_owned());
    valid
        .validate()
        .expect("a complete resume request is valid");

    let mut whole_page = request("document.pdf");
    whole_page.pdf_cursor = Some(sample_cursor(None));
    whole_page
        .validate()
        .expect("a cursor without an offset needs no page selector");
}

#[test]
fn auto_detects_common_chinese_encodings_conservatively() {
    let fixture = tempfile::tempdir().expect("fixture");
    let traditional_text = "繁體中文測試資料\n第二行內容足夠辨識\n";
    let simplified_text = "简体中文测试数据\n第二行内容足够识别\n";
    let gb18030_text = "简体中文扩展字符𠀀测试\n第二行内容足够识别\n";

    let (traditional, _, traditional_errors) = BIG5.encode(traditional_text);
    assert!(!traditional_errors);
    fs::write(fixture.path().join("traditional.txt"), traditional.as_ref()).expect("Big5 fixture");

    let (simplified, _, simplified_errors) = GBK.encode(simplified_text);
    assert!(!simplified_errors);
    fs::write(fixture.path().join("simplified.txt"), simplified.as_ref()).expect("GBK fixture");

    let (gb18030, _, gb18030_errors) = GB18030.encode(gb18030_text);
    assert!(!gb18030_errors);
    fs::write(fixture.path().join("gb18030.txt"), gb18030.as_ref()).expect("GB18030 fixture");

    let (ambiguous, _, ambiguous_errors) = BIG5.encode("中文\n");
    assert!(!ambiguous_errors);
    fs::write(fixture.path().join("ambiguous.txt"), ambiguous.as_ref())
        .expect("short Big5 fixture");

    let root = access(fixture.path());
    let cancellation = CancellationToken::new();
    let traditional =
        execute(&root, &request("traditional.txt"), &cancellation).expect("auto-detect Big5");
    assert!(traditional.contains("Encoding: Big5\n1\t繁體中文測試資料"));

    let simplified =
        execute(&root, &request("simplified.txt"), &cancellation).expect("auto-detect GBK");
    assert!(simplified.contains("Encoding: GBK\n1\t简体中文测试数据"));

    let gb18030 =
        execute(&root, &request("gb18030.txt"), &cancellation).expect("auto-detect GB18030");
    assert!(gb18030.contains("Encoding: GBK\n1\t简体中文扩展字符𠀀测试"));

    assert!(matches!(
        execute(&root, &request("ambiguous.txt"), &cancellation),
        Err(ReadError::Decode(DecodeError::UndetectedEncoding))
    ));
    let mut explicit = request("ambiguous.txt");
    explicit.encoding = Some("big5".to_owned());
    let explicit = execute(&root, &explicit, &cancellation).expect("explicit short Big5 fallback");
    assert!(explicit.contains("Encoding: Big5\n1\t中文"));
}

#[test]
fn empty_long_binary_invalid_and_out_of_range_are_bounded() {
    let fixture = tempfile::tempdir().expect("fixture");
    fs::write(fixture.path().join("empty.txt"), "").expect("empty");
    fs::write(fixture.path().join("long.txt"), "x".repeat(100_000)).expect("long");
    fs::write(fixture.path().join("binary.bin"), b"\x89PNG\r\n\x1A\nrest").expect("binary");
    fs::write(fixture.path().join("invalid.txt"), [0xFF]).expect("invalid");
    let root = access(fixture.path());
    let cancellation = CancellationToken::new();

    let empty = execute(&root, &request("empty.txt"), &cancellation).expect("empty read");
    assert_eq!(empty, "No lines.");
    let long = execute(&root, &request("long.txt"), &cancellation).expect("long read");
    assert!(long.contains("[line truncated]"));
    assert!(long.len() <= crate::output::MODEL_BYTE_LIMIT);
    assert!(matches!(
        execute(&root, &request("binary.bin"), &cancellation),
        Err(ReadError::Binary)
    ));
    assert!(matches!(
        execute(&root, &request("invalid.txt"), &cancellation),
        Err(ReadError::Decode(_))
    ));

    let mut beyond = request("empty.txt");
    beyond.start_line = Some(100);
    let output = execute(&root, &beyond, &cancellation).expect("past eof");
    assert_eq!(output, "No lines at or after start_line=100.");
}

#[test]
fn token_dense_text_preserves_the_next_line_cursor() {
    let fixture = tempfile::tempdir().expect("fixture");
    let dense = format!("{}\n", " x".repeat(12)).repeat(750);
    fs::write(fixture.path().join("dense.txt"), dense).expect("dense text");
    let root = access(fixture.path());
    let cancellation = CancellationToken::new();
    let mut page = request("dense.txt");
    page.line_count = Some(750);

    let output = execute_output(&root, &page, &cancellation).expect("bounded dense read");

    assert!(output.fits_budget(&crate::output::TestCallBudget::default()));
    assert!(output.contains("1\t x"));
    assert!(output.contains("Partial: next_start_line="));
    assert!(output.contains("(output budget)"));
    assert!(!output.contains("750\t x"));
}

#[test]
fn partial_pages_keep_the_shown_cursor_under_burst_and_item_ceilings() {
    let fixture = tempfile::tempdir().expect("fixture");
    let dense = format!("{}\n", " x".repeat(12)).repeat(750);
    fs::write(fixture.path().join("dense.txt"), dense).expect("dense text");
    let root = access(fixture.path());
    let cancellation = CancellationToken::new();
    let page = request("dense.txt");
    let prepared = prepare(&root, &page, &cancellation, budgets()).expect("prepare dense read");
    let burst_512 = crate::output::TestCallBudget {
        ceiling: 512,
        ..crate::output::TestCallBudget::default()
    };
    let crate::tools::read::Attempt::Stable(output) =
        execute_prepared_with_budget(&root, &page, prepared, &cancellation, &burst_512)
            .expect("512-token page")
    else {
        panic!("dense read must stay stable");
    };

    let next = output
        .lines()
        .find_map(|line| {
            line.strip_prefix("Partial: next_start_line=")?
                .split('.')
                .next()?
                .parse::<usize>()
                .ok()
        })
        .expect("partial cursor");
    let shown = output.lines().filter(|line| line.contains('\t')).count();
    assert_eq!(next, shown + 1);
    assert!(output.fits_call_budget(&burst_512, &cancellation));
    assert!(output.fits_call_budget(&crate::output::TestCallBudget::default(), &cancellation));
}

#[test]
fn deep_page_skips_unretained_line_prefixes_without_changing_output() {
    let fixture = tempfile::tempdir().expect("fixture");
    let mut text = String::new();
    for line in 1..=20_000 {
        use std::fmt::Write as _;
        writeln!(text, "line-{line:05}-{}", "x".repeat(64)).expect("fixture line");
    }
    fs::write(fixture.path().join("deep.txt"), text).expect("deep fixture");
    let root = access(fixture.path());
    let cancellation = CancellationToken::new();
    let mut page = request("deep.txt");
    page.start_line = Some(19_991);
    page.line_count = Some(5);

    let output = execute(&root, &page, &cancellation).expect("deep page");
    assert!(output.contains("19991\tline-19991-"));
    assert!(output.contains("19995\tline-19995-"));
    assert!(!output.contains("19990\t"));
    assert!(output.ends_with("Partial: next_start_line=19996. (line_count)"));
}

#[test]
fn candidate_budget_names_the_partial_stop_cause() {
    let fixture = tempfile::tempdir().expect("fixture");
    let mut text = String::from("keep\n");
    text.push_str(&"x".repeat(300_000));
    text.push('\n');
    text.push_str("unreached\n");
    fs::write(fixture.path().join("wide.txt"), text).expect("wide text");
    let root = access(fixture.path());
    let cancellation = CancellationToken::new();

    let output =
        execute(&root, &request("wide.txt"), &cancellation).expect("candidate-budget read");
    assert!(output.contains("1\tkeep"));
    assert!(output.contains("Partial: next_start_line=2. (read candidate budget)"));
    assert!(!output.contains("unreached"));
}

#[test]
fn read_large_file_lazy_render_preserves_output() {
    let fixture = tempfile::tempdir().expect("fixture");
    let mut text = String::new();
    for line in 1..=80_000 {
        use std::fmt::Write as _;
        writeln!(text, "line-{line:05}-{}", "x".repeat(24)).expect("fixture line");
    }
    fs::write(fixture.path().join("large.txt"), text).expect("large fixture");
    let root = access(fixture.path());
    let mut page = request("large.txt");
    page.start_line = Some(79_991);
    page.line_count = Some(5);

    let output = execute(&root, &page, &CancellationToken::new()).expect("large read");

    assert!(output.contains("79991\tline-79991-"));
    assert!(output.contains("79995\tline-79995-"));
    assert!(output.ends_with("Partial: next_start_line=79996. (line_count)"));
}

#[test]
fn read_large_file_zero_allocation_beyond_target() {
    let mut collector = crate::tools::read::text::LineCollector::new(10_001, Some(1));
    let before = "unretained\n".repeat(10_000);

    assert_eq!(
        collector.push(&before),
        crate::encoding::DecodeControl::Continue
    );
    assert_eq!(collector.allocation_state_for_test(), (0, 0));

    collector.push("target\n");
    collector.finish_eof();
    assert!(collector.allocation_state_for_test().1 > 0);
}

#[test]
fn read_4mb_fast_path_matches_stream_path() {
    let fixture = tempfile::tempdir().expect("fixture");
    let prefix = "first\nsecond\nthird\nfourth\n";
    let fast = format!("{prefix}{}", "fast\n".repeat(200_000));
    assert!(fast.len() <= 4 * 1024 * 1024);
    let stream = format!("{prefix}{}", "stream\n".repeat(700_000));
    assert!(stream.len() > 4 * 1024 * 1024);
    fs::write(fixture.path().join("fast.txt"), fast).expect("fast fixture");
    fs::write(fixture.path().join("stream.txt"), stream).expect("stream fixture");
    let root = access(fixture.path());
    let mut fast_request = request("fast.txt");
    fast_request.line_count = Some(3);
    let mut stream_request = request("stream.txt");
    stream_request.line_count = Some(3);

    let cancellation = CancellationToken::new();
    let fast_output = execute(&root, &fast_request, &cancellation).expect("fast read");
    let stream_output = execute(&root, &stream_request, &cancellation).expect("stream read");

    assert_eq!(fast_output, stream_output);
}

#[test]
fn retries_one_change_then_succeeds() {
    let fixture = tempfile::tempdir().expect("fixture");
    let path = fixture.path().join("race.txt");
    fs::write(&path, "old\n").expect("old");
    let changed = path.clone();
    BEFORE_READ_HOOK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            fs::write(&changed, "new\n").expect("change file");
        }));
    });
    let root = access(fixture.path());
    let output =
        execute(&root, &request("race.txt"), &CancellationToken::new()).expect("retry succeeds");
    assert!(output.contains("1\tnew"));
}

#[test]
fn unrestricted_external_read_preserves_change_detection() {
    let fixture = tempfile::tempdir().expect("fixture");
    let outside = tempfile::tempdir().expect("outside fixture");
    let path = outside.path().join("race.txt");
    fs::write(&path, "old\n").expect("old");
    let changed = path.clone();
    BEFORE_READ_HOOK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            fs::write(&changed, "new\n").expect("change file");
        }));
    });
    let root = access_with_scope(fixture.path(), ReadScope::Unrestricted);
    let output = execute(
        &root,
        &request(&path.to_string_lossy()),
        &CancellationToken::new(),
    )
    .expect("ambient retry succeeds");
    assert!(output.contains("1\tnew"));
}

#[test]
fn second_change_fails_explicitly() {
    let fixture = tempfile::tempdir().expect("fixture");
    let path = fixture.path().join("race.txt");
    fs::write(&path, "old\n").expect("old");
    let changed = path.clone();
    AFTER_READ_HOOK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            fs::write(&changed, "first\n").expect("first change");
            let changed_again = changed.clone();
            BEFORE_READ_HOOK.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move || {
                    fs::write(&changed_again, "second\n").expect("second change");
                }));
            });
        }));
    });
    let root = access(fixture.path());
    assert!(matches!(
        execute(&root, &request("race.txt"), &CancellationToken::new()),
        Err(ReadError::Changed)
    ));
}

#[test]
fn validation_and_directory_fail_before_content_read() {
    let fixture = tempfile::tempdir().expect("fixture");
    fs::create_dir(fixture.path().join("directory")).expect("directory");
    let root = access(fixture.path());
    let cancellation = CancellationToken::new();

    let mut invalid = request("directory");
    invalid.line_count = Some(MAX_LINE_COUNT + 1);
    assert!(matches!(
        execute(&root, &invalid, &cancellation),
        Err(ReadError::Validation(_))
    ));
    assert!(matches!(
        execute(&root, &request("directory"), &cancellation),
        Err(ReadError::Directory)
    ));
}

#[cfg(unix)]
#[test]
fn capability_allows_internal_symlink_and_blocks_escape() {
    use std::os::unix::fs::symlink;

    let fixture = tempfile::tempdir().expect("fixture");
    let outside = tempfile::tempdir().expect("outside fixture");
    fs::write(fixture.path().join("target.txt"), "inside\n").expect("target");
    fs::write(outside.path().join("secret.txt"), "outside\n").expect("secret");
    symlink("target.txt", fixture.path().join("inside-link")).expect("inside link");
    symlink(
        outside.path().join("secret.txt"),
        fixture.path().join("escape-link"),
    )
    .expect("escape link");
    let root = access(fixture.path());
    let cancellation = CancellationToken::new();

    let inside =
        execute(&root, &request("inside-link"), &cancellation).expect("internal symlink read");
    assert!(inside.contains("1\tinside"));
    assert!(execute(&root, &request("escape-link"), &cancellation).is_err());
}

#[cfg(unix)]
#[test]
fn unrestricted_external_read_rejects_explicit_symlinks() {
    use std::os::unix::fs::symlink;

    let fixture = tempfile::tempdir().expect("fixture");
    let outside = tempfile::tempdir().expect("outside fixture");
    let target = outside.path().join("target.txt");
    let link = outside.path().join("link.txt");
    fs::write(&target, "outside\n").expect("target");
    symlink(&target, &link).expect("link");
    let root = access_with_scope(fixture.path(), ReadScope::Unrestricted);
    assert!(matches!(
        execute(
            &root,
            &request(&link.to_string_lossy()),
            &CancellationToken::new()
        ),
        Err(ReadError::NotRegular)
    ));
}

#[cfg(unix)]
#[test]
fn named_pipe_and_device_paths_are_rejected_without_blocking() {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};

    let fixture = tempfile::tempdir().expect("fixture");
    let fifo = fixture.path().join("source.fifo");
    let fifo_bytes = CString::new(fifo.as_os_str().as_bytes()).expect("FIFO path");
    assert_eq!(unsafe { libc::mkfifo(fifo_bytes.as_ptr(), 0o600) }, 0);
    let root = access(fixture.path());
    let cancellation = CancellationToken::new();
    assert!(execute(&root, &request("source.fifo"), &cancellation).is_err());
    assert!(execute(&root, &request("/dev/null"), &cancellation).is_err());
}

#[cfg(windows)]
#[test]
#[ignore = "requires an elevated Windows process to create symbolic-link fixtures"]
fn windows_symlink_capability_allows_internal_link_and_blocks_reparse_escape() {
    use std::os::windows::fs::symlink_file;

    let fixture = tempfile::tempdir().expect("fixture");
    let outside = tempfile::tempdir().expect("outside fixture");
    fs::write(fixture.path().join("target.txt"), "inside\n").expect("target");
    fs::write(outside.path().join("secret.txt"), "outside\n").expect("secret");
    symlink_file("target.txt", fixture.path().join("inside-link")).expect("inside link");
    symlink_file(
        outside.path().join("secret.txt"),
        fixture.path().join("escape-link"),
    )
    .expect("escape reparse link");
    let root = access(fixture.path());
    let cancellation = CancellationToken::new();

    let inside =
        execute(&root, &request("inside-link"), &cancellation).expect("internal link read");
    assert!(inside.contains("1\tinside"));
    assert!(execute(&root, &request("escape-link"), &cancellation).is_err());

    let ambient_link = outside.path().join("ambient-link");
    symlink_file(outside.path().join("secret.txt"), &ambient_link).expect("ambient link");
    let unrestricted = access_with_scope(fixture.path(), ReadScope::Unrestricted);
    assert!(matches!(
        execute(
            &unrestricted,
            &request(&ambient_link.to_string_lossy()),
            &cancellation
        ),
        Err(ReadError::NotRegular)
    ));
}

#[cfg(windows)]
#[test]
fn windows_unicode_space_and_long_path_read() {
    let fixture = tempfile::tempdir().expect("fixture");
    let mut relative = std::path::PathBuf::from("Unicode space 界");
    for index in 0..12 {
        relative.push(format!("long-segment-{index:02}-xxxxxxxx"));
    }
    fs::create_dir_all(fixture.path().join(&relative)).expect("long path directories");
    relative.push("source file.rs");
    fs::write(fixture.path().join(&relative), "long path content\n").expect("long path file");
    assert!(fixture.path().join(&relative).as_os_str().len() > 260);
    let root = access(fixture.path());
    let output = execute(
        &root,
        &request(&relative.to_string_lossy()),
        &CancellationToken::new(),
    )
    .expect("long path read");
    assert!(output.contains("1\tlong path content"));
}
