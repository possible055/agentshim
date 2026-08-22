use super::*;

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
    let root = root_at(fixture.path());
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
    let root = root_at(fixture.path());
    let mut query = request("繁體");
    query.path = Some("large-big5.txt".to_owned());
    query.glob = None;
    query.mode = Some(GrepMode::Files);
    query.fixed_strings = Some(true);
    query.encoding = Some("big5".to_owned());

    let output =
        execute(&root, &query, 1, &CancellationToken::new()).expect("large streaming Big5 search");
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
    fs::write(fixture.path().join("malformed-big5.txt"), encoded).expect("malformed Big5 source");
    let root = root_at(fixture.path());
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
    let root = root_at(fixture.path());
    let mut query = request("繁體");
    query.path = Some("utf16.txt".to_owned());
    query.glob = None;
    query.fixed_strings = Some(true);
    query.encoding = Some("big5".to_owned());

    let output =
        execute(&root, &query, 1, &CancellationToken::new()).expect("BOM-selected UTF-16 search");
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
    let root = root_at(fixture.path());
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
    let root = root_at(fixture.path());
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
    let root = root_at(fixture.path());
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
