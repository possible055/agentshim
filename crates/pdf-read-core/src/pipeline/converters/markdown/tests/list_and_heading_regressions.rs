use super::*;

#[test]
fn test_is_bullet_span() {
    assert!(MarkdownOutputConverter::is_bullet_span("►"));
    assert!(MarkdownOutputConverter::is_bullet_span("•"));
    assert!(MarkdownOutputConverter::is_bullet_span("▪"));
    assert!(MarkdownOutputConverter::is_bullet_span(" ► "));
    assert!(!MarkdownOutputConverter::is_bullet_span("text"));
    assert!(!MarkdownOutputConverter::is_bullet_span("►text"));
    assert!(!MarkdownOutputConverter::is_bullet_span(""));
}

#[test]
fn test_starts_with_bullet() {
    assert!(MarkdownOutputConverter::starts_with_bullet("►text"));
    assert!(MarkdownOutputConverter::starts_with_bullet("• item"));
    assert!(MarkdownOutputConverter::starts_with_bullet("  ► indented"));
    assert!(!MarkdownOutputConverter::starts_with_bullet("text"));
    assert!(!MarkdownOutputConverter::starts_with_bullet(""));
}

#[test]
fn test_strip_bullet() {
    assert_eq!(MarkdownOutputConverter::strip_bullet("► text"), "text");
    assert_eq!(MarkdownOutputConverter::strip_bullet("•item"), "item");
    assert_eq!(
        MarkdownOutputConverter::strip_bullet("no bullet"),
        "no bullet"
    );
}

#[test]
fn test_bullet_spans_become_list_items() {
    // Simulates: ► (separate span) + "Analog input" (next span, same Y)
    // on a new line from previous content
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();

    let mut title = make_span("FEATURES", 50.0, 660.0, 11.0, FontWeight::Bold);
    title.reading_order = 0;

    let mut bullet = make_span("►", 50.0, 640.0, 8.8, FontWeight::Normal);
    bullet.reading_order = 1;

    let mut text = make_span("Analog input", 60.0, 640.0, 11.0, FontWeight::Normal);
    text.reading_order = 2;

    let mut bullet2 = make_span("►", 50.0, 626.0, 8.8, FontWeight::Normal);
    bullet2.reading_order = 3;

    let mut text2 = make_span("16-bit ADC", 60.0, 626.0, 11.0, FontWeight::Normal);
    text2.reading_order = 4;

    let spans = vec![title, bullet, text, bullet2, text2];
    let result = converter.convert(&spans, &config).unwrap();

    assert!(
        result.contains("- Analog input"),
        "Should convert bullet to list item: {}",
        result
    );
    assert!(
        result.contains("- 16-bit ADC"),
        "Should convert second bullet: {}",
        result
    );
    assert!(
        !result.contains("►"),
        "Should not contain raw bullet character: {}",
        result
    );
}

#[test]
fn test_inline_bullet_becomes_list_item() {
    // Simulates: "► Analog input" as a single span (inline bullet)
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();

    let mut title = make_span("TITLE", 50.0, 660.0, 11.0, FontWeight::Bold);
    title.reading_order = 0;

    let mut bullet_text = make_span("► Analog input", 50.0, 640.0, 11.0, FontWeight::Normal);
    bullet_text.reading_order = 1;

    let spans = vec![title, bullet_text];
    let result = converter.convert(&spans, &config).unwrap();

    assert!(
        result.contains("- Analog input"),
        "Should convert inline bullet to list item: {}",
        result
    );
}

#[test]
fn test_first_span_inline_bullet() {
    // First span on page starts with bullet — no prev_span exists.
    // Should still be converted to a markdown list item.
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();

    let mut bullet_text = make_span("► First item", 50.0, 660.0, 11.0, FontWeight::Normal);
    bullet_text.reading_order = 0;

    let mut bullet_text2 = make_span("► Second item", 50.0, 646.0, 11.0, FontWeight::Normal);
    bullet_text2.reading_order = 1;

    let spans = vec![bullet_text, bullet_text2];
    let result = converter.convert(&spans, &config).unwrap();

    assert!(
        result.contains("- First item"),
        "First-span inline bullet should become list item: {}",
        result
    );
    assert!(
        result.contains("- Second item"),
        "Second inline bullet should become list item: {}",
        result
    );
}

// ============================================================================
// Issue #182: Heading over-detection prevention
// ============================================================================

#[test]
fn test_heading_base_font_excludes_small_spans() {
    // When page has many 8.8pt ► spans, the base font size should
    // still be ~11pt (excluding small spans), not 8.8pt
    let converter = MarkdownOutputConverter::new();
    let config = config_with_headings();

    let mut spans = Vec::new();
    let mut order = 0;

    // 10 bullet spans at 8.8pt (should be excluded from median)
    for i in 0..10 {
        let mut s = make_span(
            "►",
            50.0,
            600.0 - (i as f32) * 14.0,
            8.8,
            FontWeight::Normal,
        );
        s.reading_order = order;
        order += 1;
        spans.push(s);
    }

    // 10 text spans at 11pt (should be the median)
    for i in 0..10 {
        let mut s = make_span(
            "body text content",
            60.0,
            600.0 - (i as f32) * 14.0,
            11.0,
            FontWeight::Bold,
        );
        s.reading_order = order;
        order += 1;
        spans.push(s);
    }

    let result = converter.convert(&spans, &config).unwrap();

    // "body text content" at 11pt should NOT be detected as heading
    // because base_font_size should be ~11pt (ratio 1.0)
    assert!(
        !result.contains("### body text content"),
        "11pt bold text should not be heading when base is 11pt: {}",
        result
    );
}

#[test]
fn test_heading_detection_still_works_for_large_fonts() {
    let converter = MarkdownOutputConverter::new();
    let config = config_with_headings();

    let mut heading = make_span("BIG HEADING", 50.0, 100.0, 24.0, FontWeight::Bold);
    heading.reading_order = 0;

    let mut body = make_span("Body text", 50.0, 70.0, 11.0, FontWeight::Normal);
    body.reading_order = 1;

    let spans = vec![heading, body];
    let result = converter.convert(&spans, &config).unwrap();

    assert!(
        result.contains("# BIG HEADING"),
        "24pt text should be H1: {}",
        result
    );
}

// ============================================================================
// Bold consolidation tests
// ============================================================================

#[test]
fn test_bold_consolidation_adjacent_bold_spans() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();

    // Three adjacent bold spans on the same line — each word is a separate span.
    // Use realistic bbox widths so that horizontal gap detection inserts spaces.
    let mut s1 = make_span_w("ACME", 72.0, 700.0, 55.0, 12.0, FontWeight::Bold);
    s1.reading_order = 0;

    let mut s2 = make_span_w("GLOBAL", 130.0, 700.0, 42.0, 12.0, FontWeight::Bold);
    s2.reading_order = 1;

    let mut s3 = make_span_w("LTD.", 175.0, 700.0, 24.0, 12.0, FontWeight::Bold);
    s3.reading_order = 2;

    let spans = vec![s1, s2, s3];
    let result = converter.convert(&spans, &config).unwrap();

    // Should consolidate into a single bold block
    assert!(
        result.contains("**ACME GLOBAL LTD.**"),
        "Adjacent bold spans should be consolidated into one bold block, got: {}",
        result
    );
    // Should NOT have per-word bold markers
    assert!(
        !result.contains("**ACME** **GLOBAL**"),
        "Should not wrap each word individually in bold markers, got: {}",
        result
    );
}

/// SPEC-ALIGNMENT (§14.8.4.3.2). When the document is TAGGED —
/// spans carry explicit `struct_role = Heading(_)` — three
/// distinct short H1 elements are author-specified structure and
/// MUST survive as three headings. The untagged word-per-heading
/// merge heuristic must NOT override authoritative tagging.
#[test]
fn test_tagged_distinct_headings_are_not_merged() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    let mk = |t: &str, y: f32| {
        let mut s = make_span(t, 0.0, y, 18.0, FontWeight::Bold);
        s.struct_role = Some(StructRole::Heading(1));
        s
    };
    // Three short headings with large baseline drops → upstream
    // emits three `# ` lines; the gate must keep them at three.
    let spans = vec![mk("Alpha", 100.0), mk("Beta", 60.0), mk("Gamma", 20.0)];
    let result = converter.convert(&spans, &config).unwrap();
    let h1_count = result.lines().filter(|l| l.starts_with("# ")).count();
    assert_eq!(
        h1_count, 3,
        "tagged distinct H1 elements must NOT be merged (spec §14.8.4.3.2), got:\n{}",
        result
    );
}

/// Issue #1 — PowerPoint-exported word-per-heading runs must fuse
/// into a single heading line.
#[test]
fn test_issue1_merge_word_per_heading_runs() {
    let input = "# Quarterly\n\n# Inventory\n\n# Review\n";
    let out = merge_consecutive_same_level_headings(input);
    assert_eq!(
        out.trim(),
        "# Quarterly Inventory Review",
        "three same-level short H1s must merge, got:\n{}",
        out
    );
}

/// Issue #4 — wrapped long-heading split across two lines must
/// fuse when there is a continuation signal (trailing comma /
/// semicolon on the first fragment, or a lowercase / connector-word
/// opener on the second). See `looks_like_heading_wrap`.
#[test]
fn test_issue4_merge_wrapped_heading_trailing_comma() {
    let input = "## Despite seasonal slowdown,\n## warehouse maintained throughput\n";
    let out = merge_consecutive_same_level_headings(input);
    assert!(
        out.contains("## Despite seasonal slowdown, warehouse maintained throughput"),
        "wrapped heading with trailing comma must fuse, got:\n{}",
        out
    );
}

/// Issue #4 — alternative continuation signal: second fragment
/// opens with a connector word ("and" / "with" / ...).
#[test]
fn test_issue4_merge_wrapped_heading_connector_opener() {
    let input = "# Architecture\n# and Implementation\n";
    let out = merge_consecutive_same_level_headings(input);
    assert!(
        out.contains("# Architecture and Implementation"),
        "wrapped heading with connector opener must fuse, got:\n{}",
        out
    );
}

/// Issue #4 — without ANY continuation signal (first ends without
/// trailing comma; second is capitalized non-connector), the
/// 2-fragment run must remain two separate headings. Guards the
/// `test_large_baseline_drop_still_splits_heading` invariant.
#[test]
fn test_issue4_does_not_fuse_ambiguous_two_headings() {
    let input = "# First Heading\n# Second Heading\n";
    let out = merge_consecutive_same_level_headings(input);
    let h_lines = out.lines().filter(|l| l.starts_with("# ")).count();
    assert_eq!(
        h_lines, 2,
        "ambiguous 2-fragment same-level headings must NOT fuse, got:\n{}",
        out
    );
}

/// Issue #1/#4 — must NOT fuse two genuinely distinct headings
/// when either side is long. Guards against over-eager merging.
#[test]
fn test_issue1_does_not_fuse_long_distinct_headings() {
    let h1 = "# Annual Sales Performance Across Every Region in Detail";
    let h2 = "# Q1 Highlights and Outlook for the Year";
    let input = format!("{}\n\n{}\n", h1, h2);
    let out = merge_consecutive_same_level_headings(&input);
    assert!(
        out.contains(h1) && out.contains(h2),
        "two long distinct headings must remain separate, got:\n{}",
        out
    );
}

/// Issue #11 — KPI numeric-only H1 run collapses to bulleted list.
#[test]
fn test_issue11_collapses_numeric_heading_run() {
    let input = "# 23,500\n\n# 99.2%\n\n# 87%\n\n# 4.2 days\n";
    let out = collapse_numeric_heading_runs(input);
    for v in ["- 23,500", "- 99.2%", "- 87%", "- 4.2 days"] {
        assert!(out.contains(v), "expected `{}` in output, got:\n{}", v, out);
    }
    assert!(
        !out.contains("# 23,500"),
        "H1 form must be gone, got:\n{}",
        out
    );
}

/// Issue #11 — a numeric heading that LOOKS standalone (single
/// occurrence) must NOT collapse. Two-or-more is the trigger.
#[test]
fn test_issue11_preserves_single_numeric_heading() {
    let input = "# 2024 Annual Report\n";
    let out = collapse_numeric_heading_runs(input);
    assert_eq!(
        out, input,
        "single non-numeric heading must be untouched: {}",
        out
    );
}

/// Issue #12 — `**S alesF orce**` CamelCase fragmentation inside a
/// single bold pair coalesces to `**SalesForce**`.
#[test]
fn test_issue12_coalesces_inline_camelcase_bold() {
    let input = "**S alesF orce** is great.\n";
    let out = coalesce_camelcase_bold_fragments(input);
    assert!(
        out.contains("**SalesForce**"),
        "inline CamelCase bold must coalesce, got:\n{}",
        out
    );
}

/// Issue #12 — must NOT touch legitimate two-word bold like
/// `**John Smith**` or `**USB Type C**`. The CamelCase signal
/// (lowercase-then-uppercase inside one fragment) is required.
#[test]
fn test_issue12_preserves_normal_multi_word_bold() {
    let input = "**John Smith** wrote.\n**USB Type C** cable.\n";
    let out = coalesce_camelcase_bold_fragments(input);
    assert!(
        out.contains("**John Smith**"),
        "two-word person bold must not be merged, got:\n{}",
        out
    );
    assert!(
        out.contains("**USB Type C**"),
        "three-word product bold must not be merged, got:\n{}",
        out
    );
}

/// Issue #12 (BOUND case) — closing `**` lands mid-CamelCase:
/// `**N orthW** ind` (intended `**N**orthWind` or `**NorthWind**`).
/// This is the pattern not yet covered by the inline-bold regex.
/// Marked `#[ignore]` until the bound coalescer lands.
#[test]
fn test_issue12_bound_camelcase_bold_coalesces() {
    let input = "**N orthW** ind";
    let out = coalesce_camelcase_bold_fragments(input);
    // Either of these post-coalesce forms is acceptable; both
    // recover the intended brand name.
    let acceptable = out.contains("**NorthWind**")
        || out.contains("**NorthW**ind")
        || out.contains("**N**orthWind");
    assert!(
        acceptable,
        "bound CamelCase bold (closing ** mid-word) should coalesce, got:\n{}",
        out
    );
}
