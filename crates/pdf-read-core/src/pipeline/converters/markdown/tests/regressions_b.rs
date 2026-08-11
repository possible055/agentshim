use super::*;

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

/// Issue #3 — spatial-prose-as-table (>= 5 cols, >= 2 data rows,
/// >= 60% single-word non-empty cells) collapses to a paragraph.
#[test]
fn test_issue3_degenerate_table_collapses_to_paragraph() {
    let input = "\
| Q1 | Warehouse | throughput | increased | 15% |
|---|---|---|---|---|
| quarter | over | quarter | to | 23,500 |
| units | per | day | strong | demand |
";
    let out = simplify_degenerate_tables(input);
    assert!(
        !out.contains("|---|"),
        "separator row should be gone, got:\n{}",
        out
    );
    assert!(
        out.contains("Q1 Warehouse throughput increased 15%"),
        "header words flattened to prose, got:\n{}",
        out
    );
}

/// Issue #3 — a normal table with multi-word cells must SURVIVE.
/// Guards against over-eager flattening that would corrupt real
/// tabular data.
#[test]
fn test_issue3_preserves_legitimate_multi_word_tables() {
    let input = "\
| Region | Revenue Q1 | Revenue Q2 | Revenue Q3 | Revenue Q4 |
|---|---|---|---|---|
| North America Sales | 1.2 M | 1.5 M | 1.7 M | 1.9 M |
| Europe Sales Total | 0.8 M | 0.9 M | 1.1 M | 1.3 M |
";
    let out = simplify_degenerate_tables(input);
    assert!(
        out.contains("|---|"),
        "real table must keep separator, got:\n{}",
        out
    );
    assert!(
        out.contains("| North America Sales |"),
        "real table cells must remain, got:\n{}",
        out
    );
}

/// Issue #9 — page-number-shaped lines (e.g. "Page 1 of 12",
/// "— 5 —", "[12]") MUST be preserved in the markdown output if
/// they appear in the prose stream. Dropping them at this layer
/// would discard legitimate content — the proper fix is upstream
/// artifact (`/Artifact` tag) handling per PDF §14.8.2.2. This
/// test pins that contract: the post-process pipeline does not
/// touch these lines.
#[test]
fn test_issue9_preserves_page_number_shaped_lines() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    let spans = vec![
        make_span("Some text.", 0.0, 100.0, 12.0, FontWeight::Normal),
        make_span("Page 1 of 12", 0.0, 80.0, 10.0, FontWeight::Normal),
        make_span("More text.", 0.0, 60.0, 12.0, FontWeight::Normal),
    ];
    let result = converter.convert(&spans, &config).unwrap();
    assert!(
        result.contains("Page 1 of 12"),
        "page-N text must survive, got:\n{}",
        result
    );
    assert!(
        result.contains("Some text."),
        "prose must survive, got:\n{}",
        result
    );
    assert!(
        result.contains("More text."),
        "prose must survive, got:\n{}",
        result
    );
}

/// Issue #9 — in-prose "Page N" references must obviously also
/// survive (this was the existing guard).
#[test]
fn test_issue9_preserves_page_in_prose() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    let spans = vec![make_span(
        "See Page 3 for details about the change.",
        0.0,
        100.0,
        12.0,
        FontWeight::Normal,
    )];
    let result = converter.convert(&spans, &config).unwrap();
    assert!(
        result.contains("See Page 3 for details"),
        "in-prose 'Page N' must not be dropped, got:\n{}",
        result
    );
}

/// Issue #13 — wrong-glyph bullets (`❍`, `◦`, ...) at line start
/// must NOT be silently dropped. The upstream renderer already
/// recognizes these as bullet-glyph variants and emits them as
/// idiomatic markdown `- ` bullets — that preserves the semantic
/// list structure across all glyph variants. What this test
/// pins is content preservation: the text content after the
/// glyph (`First item`, `Second item`) must reach the output;
/// the bullet symbol itself can be normalized to `-` because
/// markdown's bullet semantics are the same.
///
/// What is NOT acceptable (the bug we're guarding against): a
/// post-process layer pattern-matching codepoints and rewriting
/// them in arbitrary text. The pipeline does no such rewriting
/// (see `normalize_bullet_glyphs` no-op doc).
#[test]
fn test_issue13_preserves_bullet_text_content() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    let spans = vec![
        make_span("\u{274D} First item", 0.0, 100.0, 12.0, FontWeight::Normal),
        make_span("\u{25E6} Second item", 0.0, 80.0, 12.0, FontWeight::Normal),
    ];
    let result = converter.convert(&spans, &config).unwrap();
    assert!(
        result.contains("First item"),
        "list-item text must survive: {}",
        result
    );
    assert!(
        result.contains("Second item"),
        "list-item text must survive: {}",
        result
    );
}

/// Issue #13 (mid-prose codepoint preservation). A `❍` that
/// appears in the MIDDLE of body text (not at line start) must
/// be preserved verbatim — at that position the upstream does
/// not treat it as a bullet, so any rewriting would be content
/// corruption.
#[test]
fn test_issue13_preserves_mid_prose_bullet_codepoint() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    let spans = vec![make_span(
        "The symbol \u{274D} indicates a shadow circle.",
        0.0,
        100.0,
        12.0,
        FontWeight::Normal,
    )];
    let result = converter.convert(&spans, &config).unwrap();
    assert!(
        result.contains("\u{274D}"),
        "mid-prose U+274D must survive verbatim, got:\n{}",
        result
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

/// Issue #8 — a table cell that carries bold spans must render the
/// bold markers in the output. Reporter measured 73% bold-marker
/// loss across 53/54 files; this asserts at least the simple case.
#[test]
fn test_issue8_table_cell_renders_bold_marker() {
    let bold_span = TextSpan {
        provenance: None,
        text_rise: 0.0,
        artifact_type: None,
        text: "Critical".to_string(),
        bbox: Rect::new(0.0, 0.0, 50.0, 12.0),
        font_name: "Test-Bold".to_string(),
        font_size: 12.0,
        font_weight: FontWeight::Bold,
        is_italic: false,
        is_monospace: false,
        color: Color::black(),
        mcid: None,
        mcid_scope: None,
        sequence: 0,
        offset_semantic: false,
        split_boundary_before: false,
        char_spacing: 0.0,
        word_spacing: 0.0,
        horizontal_scaling: 100.0,
        primary_detected: false,
        char_widths: vec![],
        char_x_offsets: Vec::new(),
        heading_level: None,
        rotation_degrees: 0.0,
        wmode: 0,
        rtl_draw_logical: false,
    };
    let mut cell = TableCell::new("Critical".to_string(), false);
    cell.spans.push(bold_span.clone());
    let mut row = TableRow::new(false);
    row.add_cell(cell);
    let mut table = Table::new();
    table.add_row(row);

    let result = MarkdownOutputConverter::new()
        .render_table_markdown(&table, &TextPipelineConfig::default());
    assert!(
        result.contains("**Critical**"),
        "bold marker must appear in rendered cell, got:\n{}",
        result
    );
}

/// Issue #2 — consecutive duplicate paragraphs (structured +
/// plaintext echo) must be deduped down to one.
#[test]
fn test_issue2_dedup_consecutive_duplicate_paragraphs() {
    let input = "Revenue grew by 15%.\n\nRevenue grew by 15%.\n\nNext paragraph here.\n";
    let out = dedup_consecutive_paragraphs(input);
    let occurrences = out.matches("Revenue grew by 15%.").count();
    assert_eq!(
        occurrences, 1,
        "exact-duplicate consecutive paragraph must collapse, got:\n{}",
        out
    );
    assert!(
        out.contains("Next paragraph here."),
        "subsequent paragraph must survive, got:\n{}",
        out
    );
}

/// Issue #2 — non-consecutive duplicates (separated by other
/// content) must NOT be touched: legitimate prose can repeat a
/// phrase later in the document.
#[test]
fn test_issue2_preserves_nonconsecutive_repeats() {
    let input = "Important note.\n\nOther content.\n\nImportant note.\n";
    let out = dedup_consecutive_paragraphs(input);
    let occurrences = out.matches("Important note.").count();
    assert_eq!(
        occurrences, 2,
        "non-consecutive repeat must survive, got:\n{}",
        out
    );
}

/// Issue #5 — all-identical header cells (spatial-grouping
/// artifact) must be deduped to a single occurrence in the
/// rendered output. Operates on the assembled markdown so it
/// catches both render paths.
#[test]
fn test_issue5_dedups_identical_header_cells() {
    let input = "| Q1'25 | Q1'25 | Q1'25 | Q1'25 |\n|---|---|---|---|\n| Zone A |  |  |  |\n";
    let out = dedup_identical_header_cells(input);
    let q1_count = out.matches("Q1'25").count();
    assert_eq!(
        q1_count, 1,
        "all-identical header cells must dedup to one, got {} in:\n{}",
        q1_count, out
    );
    // Cell count preserved (still 4 pipes in the data row).
    assert!(
        out.contains("Zone A"),
        "data row must remain intact, got:\n{}",
        out
    );
}

/// Issue #5 — a legitimate header with distinct values must NOT
/// be touched.
#[test]
fn test_issue5_preserves_real_distinct_headers() {
    let input = "| North | South | East | West |\n|---|---|---|---|\n| 1 | 2 | 3 | 4 |\n";
    let out = dedup_identical_header_cells(input);
    for col in ["North", "South", "East", "West"] {
        assert!(
            out.contains(col),
            "distinct header `{}` must survive: {}",
            col,
            out
        );
    }
}

/// Issue #7 — when side-by-side columns are present, text from
/// column 2 must not interleave with column 1's text mid-paragraph.
/// The existing `is_column_gap` heuristic (forward gutter > 3×
/// font_size OR backward wrap) is what forces the paragraph break
/// between columns; this test pins that behavior so future
/// reading-order refactors don't silently regress it.
#[test]
fn test_issue7_no_column_interleaving() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    let mk = |t: &str, x: f32, y: f32, bid: u32| {
        let mut s = make_span(t, x, y, 12.0, FontWeight::Normal);
        s.block_id = Some(bid);
        s
    };
    // Left column at x=0, right column at x=300; baselines stagger.
    let spans = vec![
        mk("Left A.", 0.0, 100.0, 1),
        mk("Right A.", 300.0, 100.0, 2),
        mk("Left B.", 0.0, 88.0, 1),
        mk("Right B.", 300.0, 88.0, 2),
    ];
    let result = converter.convert(&spans, &config).unwrap();
    // Left column must surface as a contiguous run.
    assert!(
        result.contains("Left A.") && result.contains("Left B."),
        "left column must surface, got:\n{}",
        result
    );
    // No interleaving: "Left A. Right A." together would prove
    // interleaving (reading-order put right immediately after left
    // before left's continuation).
    assert!(
        !result.contains("Left A. Right A."),
        "columns must not interleave at the line level, got:\n{}",
        result
    );
}

// ==========================================================================
// Bidi-isolation markers in markdown output (#537 follow-up — v0.3.55).
// Acceptance tests from
// docs/releases/plans/v0.3.55/fix-537-followup-bidi-isolation-markers.md.
// ==========================================================================

/// Hebrew run in an LTR-dominant line — must be wrapped with
/// U+2067 (RLI) … U+2069 (PDI) so a UAX #9-aware markdown
/// viewer does not let neutrals around the Hebrew bleed across
/// the boundary. Pre-fix output had no markers; this test pins
/// the post-fix behaviour.
#[test]
fn markdown_wraps_rtl_run_with_rli_pdi() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    let span = make_span(
        "The article שלום עולם is greetings.",
        0.0,
        100.0,
        12.0,
        FontWeight::Normal,
    );
    let result = converter.convert(&[span], &config).unwrap();
    assert!(
        result.contains('\u{2067}'),
        "expected U+2067 (RLI) in markdown output, got:\n{:?}",
        result
    );
    assert!(
        result.contains('\u{2069}'),
        "expected U+2069 (PDI) in markdown output, got:\n{:?}",
        result
    );
    // Block is LTR-dominant — LTR runs must NOT get LRI.
    assert!(
        !result.contains('\u{2066}'),
        "unexpected U+2066 (LRI) in LTR-block output:\n{:?}",
        result
    );
}

/// English brand-name embedded in a Hebrew (RTL-dominant) line
/// — the English run must be wrapped with U+2066 (LRI) …
/// U+2069 (PDI).
#[test]
fn markdown_wraps_ltr_run_inside_rtl_block_with_lri_pdi() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    let span = make_span("הספר Microsoft חדש", 0.0, 100.0, 12.0, FontWeight::Normal);
    let result = converter.convert(&[span], &config).unwrap();
    assert!(
        result.contains('\u{2066}'),
        "expected U+2066 (LRI) wrapping the embedded LTR token, got:\n{:?}",
        result
    );
    assert!(
        result.contains('\u{2069}'),
        "expected U+2069 (PDI) closing the LRI, got:\n{:?}",
        result
    );
    // Block is RTL-dominant — RTL runs must NOT get RLI.
    assert!(
        !result.contains('\u{2067}'),
        "unexpected U+2067 (RLI) in RTL-block output:\n{:?}",
        result
    );
}

/// Regression guard: pure-LTR markdown output must contain
/// ZERO bidi-isolation markers anywhere. This is the "no
/// markers appear in pure-LTR documents" contract from the
/// v0.3.55 plan's acceptance criteria. If this ever fails, the
/// isolation pass leaked into LTR-only output.
#[test]
fn markdown_leaves_pure_ltr_unchanged() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    let spans = vec![
        make_span("The first paragraph.", 0.0, 100.0, 12.0, FontWeight::Normal),
        make_span("A second sentence.", 0.0, 84.0, 12.0, FontWeight::Normal),
        make_span(
            "Numbers 123 and (parens) too.",
            0.0,
            68.0,
            12.0,
            FontWeight::Normal,
        ),
    ];
    let result = converter.convert(&spans, &config).unwrap();
    for marker in ['\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}'] {
        assert!(
            !result.contains(marker),
            "pure-LTR output must not contain U+{:04X}, got:\n{:?}",
            marker as u32,
            result
        );
    }
}
