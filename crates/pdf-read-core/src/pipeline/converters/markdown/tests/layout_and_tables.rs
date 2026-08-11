use super::*;

/// WS2.5: a list item whose bullet sits further right than the top-level
/// markers gets a 2-space indent per level; a flat list is unindented
/// (byte-identical to before).
#[test]
fn md_nested_list_indents_by_marker_x() {
    let config = TextPipelineConfig::default();
    let converter = MarkdownOutputConverter::new();

    let nested = converter
        .convert(
            &[
                make_span("• Alpha", 50.0, 700.0, 12.0, FontWeight::Normal),
                make_span("• Beta", 50.0, 686.0, 12.0, FontWeight::Normal),
                make_span("• Gamma", 80.0, 672.0, 12.0, FontWeight::Normal),
            ],
            &config,
        )
        .unwrap();
    assert!(
        nested.contains("- Alpha"),
        "top-level item unindented: {nested:?}"
    );
    assert!(
        nested.contains("  - Gamma"),
        "deeper-x item gets a 2-space indent: {nested:?}"
    );

    let flat = converter
        .convert(
            &[
                make_span("• One", 50.0, 700.0, 12.0, FontWeight::Normal),
                make_span("• Two", 50.0, 686.0, 12.0, FontWeight::Normal),
            ],
            &config,
        )
        .unwrap();
    assert!(
        !flat.contains("  - "),
        "a flat list must not be indented: {flat:?}"
    );
}

/// WS2.4: multi-level numbered section titles promote to headings at their
/// dot-depth; list markers and non-headings do not.
#[test]
fn md_numbered_heading_promotion() {
    let f = MarkdownOutputConverter::numbered_heading_level;
    assert_eq!(f("2.1.3 Results and discussion"), Some(3));
    assert_eq!(f("2.1 Method"), Some(2));
    assert_eq!(f("4.2.1.5 Deep"), Some(4)); // 3 dots → level 4
                                            // Not headings:
    assert_eq!(
        f("1. First list item"),
        None,
        "single-dot+space is a list marker"
    );
    assert_eq!(f("3 Discussion"), None, "no dot is too ambiguous");
    assert_eq!(f("Introduction"), None, "no number");
    assert_eq!(f("2.1.3"), None, "number with no title");
    assert_eq!(f("2020.05.13"), None, "date-like, no alpha title");
}

// ---- Heading size-ratio promotion: body text must not be over-promoted ----

#[test]
fn heading_ratio_does_not_promote_body_text() {
    // A document with a single clear heading (14pt) over 10pt body. The body
    // spans (10pt) must never be promoted to a heading — only the 14pt title,
    // which the size-ratio heuristic recovers.
    let converter = MarkdownOutputConverter::new();
    let mut config = TextPipelineConfig::default();
    config.output.detect_headings = true;
    let mut spans = vec![make_span_w(
        "Title Here",
        0.0,
        800.0,
        100.0,
        14.0,
        FontWeight::Normal,
    )];
    for i in 0..8 {
        spans.push(make_span_w(
            "plain body sentence that is clearly not a heading at all",
            0.0,
            700.0 - (i as f32) * 12.0,
            260.0,
            10.0,
            FontWeight::Normal,
        ));
    }
    let md = converter.convert(&spans, &config).unwrap();
    // No body line should have become a heading (no stray leading '#').
    for line in md.lines() {
        if line.starts_with('#') {
            assert!(
                line.contains("Title Here"),
                "body text wrongly promoted to heading: {line:?}"
            );
        }
    }
}

// ---- WS2.7 footnote detection ----

#[test]
fn footnote_marker_with_matching_bottom_def_emits_reference() {
    // Body text ending in a raised superscript "1" plus a page-bottom
    // definition line "1 Smith et al. 2019" → inline `[^1]` and a
    // trailing `[^1]: …` definition.
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    let spans = vec![
        make_span_w("As shown", 0.0, 700.0, 60.0, 11.0, FontWeight::Normal),
        // raised (y 702 > 700) and small (7pt < 0.75×11)
        make_span_w("1", 60.0, 702.0, 5.0, 7.0, FontWeight::Normal),
        // page-bottom definition block (bottom 18% band)
        make_span_w("1", 0.0, 50.0, 5.0, 7.0, FontWeight::Normal),
        make_span_w(
            "Smith et al. 2019",
            10.0,
            50.0,
            120.0,
            7.0,
            FontWeight::Normal,
        ),
    ];
    let md = converter.convert(&spans, &config).unwrap();
    assert!(
        md.contains("As shown[^1]"),
        "inline reference missing: {md:?}"
    );
    assert!(
        md.contains("[^1]: Smith et al. 2019"),
        "definition missing: {md:?}"
    );
}

#[test]
fn footnote_symbol_marker_with_matching_def_emits_sequential_id() {
    // A symbol marker (`*`) is confirmed by a matching bottom def and
    // assigned the sequential id `1`. Also guards the noise-filter: a
    // lone `*` span must survive because it is a confirmed marker.
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    let spans = vec![
        make_span_w("See note", 0.0, 700.0, 60.0, 11.0, FontWeight::Normal),
        make_span_w("*", 60.0, 702.0, 5.0, 7.0, FontWeight::Normal),
        make_span_w("*", 0.0, 50.0, 5.0, 7.0, FontWeight::Normal),
        make_span_w(
            "Important caveat",
            10.0,
            50.0,
            100.0,
            7.0,
            FontWeight::Normal,
        ),
    ];
    let md = converter.convert(&spans, &config).unwrap();
    assert!(
        md.contains("See note[^1]"),
        "inline reference missing: {md:?}"
    );
    assert!(
        md.contains("[^1]: Important caveat"),
        "definition missing: {md:?}"
    );
}

#[test]
fn footnote_superscript_without_matching_def_left_unchanged() {
    // A raised superscript "1" whose only bottom block starts with a
    // DIFFERENT token ("2") must not be rewritten — neither half is
    // confirmed, so no footnote syntax is emitted at all.
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    let spans = vec![
        make_span_w("As shown", 0.0, 700.0, 60.0, 11.0, FontWeight::Normal),
        make_span_w("1", 60.0, 702.0, 5.0, 7.0, FontWeight::Normal),
        make_span_w("2", 0.0, 50.0, 5.0, 7.0, FontWeight::Normal),
        make_span_w("Unrelated note", 10.0, 50.0, 100.0, 7.0, FontWeight::Normal),
    ];
    let md = converter.convert(&spans, &config).unwrap();
    assert!(!md.contains("[^"), "unexpected footnote syntax: {md:?}");
    assert!(md.contains("As shown"), "body text lost: {md:?}");
}

#[test]
fn footnote_ordinal_superscript_not_treated_as_footnote() {
    // "May 5th" with a superscript ordinal "th" must never be treated
    // as a footnote: the token is letters, not a digit/symbol, so it
    // is never even a candidate marker.
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    let spans = vec![
        make_span_w("May", 0.0, 700.0, 30.0, 11.0, FontWeight::Normal),
        make_span_w("5", 30.0, 700.0, 8.0, 11.0, FontWeight::Normal),
        make_span_w("th", 38.0, 702.0, 8.0, 7.0, FontWeight::Normal),
        // a bottom block that would confirm a real footnote is present
        // but cannot match a letter-token "th".
        make_span_w("1", 0.0, 50.0, 5.0, 7.0, FontWeight::Normal),
        make_span_w("Footer note", 10.0, 50.0, 90.0, 7.0, FontWeight::Normal),
    ];
    let md = converter.convert(&spans, &config).unwrap();
    assert!(!md.contains("[^"), "ordinal wrongly footnoted: {md:?}");
}

#[test]
fn md_wrapped_hyphen_line_joins_without_space() {
    // A line that wraps mid-word at a hyphen joins WITHOUT a space, and the hyphen
    // goes with it: it belongs to the line break, not to the word.
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    let spans = vec![
        make_span_w("implementa-", 0.0, 100.0, 40.0, 10.0, FontWeight::Normal),
        make_span_w("tion", 0.0, 89.0, 40.0, 10.0, FontWeight::Normal),
    ];
    let md = converter.convert(&spans, &config).unwrap();
    assert!(md.contains("implementation"), "not rejoined: {md:?}");
    assert!(
        !md.contains("implementa- tion"),
        "spurious space after hyphen: {md:?}"
    );
}

/// The other half of the rule: a hyphen the author wrote survives the same wrap.
/// This is the case that keeps `pre-training` and `self-attention` intact on the
/// academic PDFs the read path is aimed at.
#[test]
fn md_wrapped_hyphen_keeps_an_authored_compound() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    for (head, tail, joined) in [
        ("pre-", "training", "pre-training"),
        ("self-", "attention", "self-attention"),
        ("Fine-", "Tuning", "Fine-Tuning"),
        ("2019-", "2020", "2019-2020"),
    ] {
        let spans = vec![
            make_span_w(head, 0.0, 100.0, 40.0, 10.0, FontWeight::Normal),
            make_span_w(tail, 0.0, 89.0, 40.0, 10.0, FontWeight::Normal),
        ];
        let md = converter.convert(&spans, &config).unwrap();
        assert!(
            md.contains(joined),
            "{head:?}+{tail:?} lost its hyphen: {md:?}"
        );
    }
}

#[test]
fn md_wrapped_plain_line_keeps_space() {
    // Guard: a normal (non-hyphen) wrap still gets its joining space.
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    let spans = vec![
        make_span_w("hello", 0.0, 100.0, 40.0, 10.0, FontWeight::Normal),
        make_span_w("world", 0.0, 89.0, 40.0, 10.0, FontWeight::Normal),
    ];
    let md = converter.convert(&spans, &config).unwrap();
    assert!(md.contains("hello world"), "lost joining space: {md:?}");
}

#[test]
fn md_rtl_interword_punct_separator_is_kept_and_not_paragraph_broken() {
    // A right-to-left line emits the comma/period between two words as its
    // own space-bearing span (" ,"). It must (a) survive the noise filter
    // so the flanking words are NOT glued, and (b) not be treated as a
    // same-baseline column gap that shatters the line into one paragraph
    // per word. Mirrors the wiki-cat-he front-matter line
    // `…הטורפים, ממשפחת…`.
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    // RTL reading order steps left (decreasing x), same baseline.
    let spans = vec![
        make_span_w("הטורפים", 200.0, 100.0, 60.0, 13.0, FontWeight::Normal),
        make_span_w(" ,", 190.0, 100.0, 7.0, 13.0, FontWeight::Normal),
        make_span_w("ממשפחת", 130.0, 100.0, 55.0, 13.0, FontWeight::Normal),
    ];
    let md = converter.convert(&spans, &config).unwrap();
    assert!(md.contains("הטורפים"), "lost first word: {md:?}");
    assert!(md.contains("ממשפחת"), "lost second word: {md:?}");
    assert!(
        !md.contains("הטורפיםממשפחת"),
        "words glued — separator was dropped: {md:?}"
    );
    let para_count = md
        .trim()
        .split("\n\n")
        .filter(|p| !p.trim().is_empty())
        .count();
    assert_eq!(para_count, 1, "RTL line shattered into paragraphs: {md:?}");
}

#[test]
fn test_empty_spans() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    let result = converter.convert(&[], &config).unwrap();
    assert_eq!(result, "");
}

#[test]
fn test_single_span() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    let spans = vec![make_span(
        "Hello world",
        0.0,
        100.0,
        12.0,
        FontWeight::Normal,
    )];
    let result = converter.convert(&spans, &config).unwrap();
    assert_eq!(result, "Hello world\n");
}

#[test]
fn test_bold_text() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    let spans = vec![make_span("Bold text", 0.0, 100.0, 12.0, FontWeight::Bold)];
    let result = converter.convert(&spans, &config).unwrap();
    assert_eq!(result, "**Bold text**\n");
}

#[test]
fn test_whitespace_bold_conservative() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    // Whitespace-only bold should not have markers in conservative mode
    let spans = vec![make_span("   ", 0.0, 100.0, 12.0, FontWeight::Bold)];
    let result = converter.convert(&spans, &config).unwrap();
    // Should not contain bold markers
    assert!(!result.contains("**"));
}

#[test]
fn test_convert_with_tables_renders_markdown_table() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();

    let mut table = Table::new();
    table.bbox = Some(Rect::new(10.0, 50.0, 200.0, 100.0));
    table.col_count = 2;
    table.has_header = true;

    let mut header = TableRow::new(true);
    header.add_cell(TableCell::new("Name".to_string(), true));
    header.add_cell(TableCell::new("Value".to_string(), true));
    table.add_row(header);

    let mut data = TableRow::new(false);
    data.add_cell(TableCell::new("A".to_string(), false));
    data.add_cell(TableCell::new("1".to_string(), false));
    table.add_row(data);

    let result = converter
        .convert_with_tables(&[], &[table], &config)
        .unwrap();

    assert!(result.contains("| Name |"));
    assert!(result.contains("| Value |"));
    assert!(result.contains("---|"));
    assert!(result.contains("| A |"));
    assert!(result.contains("| 1 |"));
}

// ============================================================================
// render_table_markdown() tests
// ============================================================================

#[test]
fn test_render_table_markdown_empty() {
    let table = Table::new();
    let result = MarkdownOutputConverter::new()
        .render_table_markdown(&table, &crate::pipeline::TextPipelineConfig::default());
    assert_eq!(result, "");
}

#[test]
fn test_render_table_markdown_single_row_no_header() {
    let mut table = Table::new();
    let mut row = TableRow::new(false);
    row.add_cell(TableCell::new("A".to_string(), false));
    row.add_cell(TableCell::new("B".to_string(), false));
    table.add_row(row);

    let result = MarkdownOutputConverter::new()
        .render_table_markdown(&table, &crate::pipeline::TextPipelineConfig::default());
    assert!(result.contains("| A |"));
    assert!(result.contains("| B |"));
    // First row treated as header by default in markdown
    assert!(result.contains("---|"));
}

#[test]
fn test_render_table_markdown_with_colspan() {
    let mut table = Table::new();
    table.has_header = true;
    let mut header = TableRow::new(true);
    header.add_cell(TableCell::new("Wide".to_string(), true).with_colspan(2));
    table.add_row(header);

    let mut data = TableRow::new(false);
    data.add_cell(TableCell::new("Left".to_string(), false));
    data.add_cell(TableCell::new("Right".to_string(), false));
    table.add_row(data);

    let result = MarkdownOutputConverter::new()
        .render_table_markdown(&table, &crate::pipeline::TextPipelineConfig::default());
    // Colspan cell should produce extra | separators
    assert!(result.contains("| Wide |"));
    assert!(result.contains("---|---|"));
}

#[test]
fn test_render_table_markdown_escapes_pipes() {
    let mut table = Table::new();
    let mut row = TableRow::new(false);
    row.add_cell(TableCell::new("A|B".to_string(), false));
    table.add_row(row);

    let result = MarkdownOutputConverter::new()
        .render_table_markdown(&table, &crate::pipeline::TextPipelineConfig::default());
    assert!(
        result.contains("A\\|B"),
        "Pipes should be backslash-escaped: {}",
        result
    );
}

#[test]
fn test_render_table_markdown_replaces_newlines() {
    let mut table = Table::new();
    let mut row = TableRow::new(false);
    row.add_cell(TableCell::new("Line1\nLine2".to_string(), false));
    table.add_row(row);

    let result = MarkdownOutputConverter::new()
        .render_table_markdown(&table, &crate::pipeline::TextPipelineConfig::default());
    assert!(
        !result.contains("Line1\nLine2"),
        "Newlines in cells should be replaced"
    );
    assert!(result.contains("Line1 Line2"));
}

#[test]
fn test_render_table_markdown_trims_whitespace() {
    let mut table = Table::new();
    let mut row = TableRow::new(false);
    row.add_cell(TableCell::new("  padded  ".to_string(), false));
    table.add_row(row);

    let result = MarkdownOutputConverter::new()
        .render_table_markdown(&table, &crate::pipeline::TextPipelineConfig::default());
    assert!(result.contains("| padded |"));
}

#[test]
fn test_render_table_markdown_multiple_header_rows() {
    let mut table = Table::new();
    table.has_header = true;

    let mut h1 = TableRow::new(true);
    h1.add_cell(TableCell::new("H1".to_string(), true));
    table.add_row(h1);

    let mut h2 = TableRow::new(true);
    h2.add_cell(TableCell::new("H2".to_string(), true));
    table.add_row(h2);

    let mut d1 = TableRow::new(false);
    d1.add_cell(TableCell::new("D1".to_string(), false));
    table.add_row(d1);

    let result = MarkdownOutputConverter::new()
        .render_table_markdown(&table, &crate::pipeline::TextPipelineConfig::default());
    // Separator should appear after last header row (row_idx == 1)
    let lines: Vec<&str> = result.lines().collect();
    assert_eq!(lines.len(), 4); // H1, H2, separator, D1
    assert!(lines[2].contains("---|"));
}

// ============================================================================
// span_in_table() tests
// ============================================================================

#[test]
fn test_span_in_table_match() {
    let span = make_span("text", 50.0, 70.0, 12.0, FontWeight::Normal);

    let mut table = Table::new();
    table.bbox = Some(Rect::new(10.0, 50.0, 200.0, 100.0));

    assert_eq!(span_in_table(&span, &[table]), Some(0));
}

#[test]
fn test_span_in_table_no_match() {
    let span = make_span("text", 500.0, 500.0, 12.0, FontWeight::Normal);

    let mut table = Table::new();
    table.bbox = Some(Rect::new(10.0, 50.0, 200.0, 100.0));

    assert_eq!(span_in_table(&span, &[table]), None);
}

#[test]
fn test_span_in_table_none_bbox() {
    let span = make_span("text", 50.0, 70.0, 12.0, FontWeight::Normal);

    let table = Table::new(); // No bbox
    assert_eq!(span_in_table(&span, &[table]), None);
}

#[test]
fn test_span_in_table_tolerance() {
    // Span at bbox edge minus tolerance (2.0)
    let span = make_span("text", 8.5, 48.5, 12.0, FontWeight::Normal);

    let mut table = Table::new();
    table.bbox = Some(Rect::new(10.0, 50.0, 200.0, 100.0));

    assert_eq!(
        span_in_table(&span, &[table]),
        Some(0),
        "Should match within tolerance"
    );
}

#[test]
fn test_span_in_table_multiple_tables() {
    let span = make_span("text", 350.0, 70.0, 12.0, FontWeight::Normal);

    let mut t1 = Table::new();
    t1.bbox = Some(Rect::new(10.0, 50.0, 200.0, 100.0));

    let mut t2 = Table::new();
    t2.bbox = Some(Rect::new(300.0, 50.0, 200.0, 100.0));

    assert_eq!(span_in_table(&span, &[t1, t2]), Some(1));
}

// ============================================================================
// convert_with_tables() integration tests
// ============================================================================

#[test]
fn test_convert_with_tables_mixed_content() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();

    // Text before the table
    let mut span_before = make_span("Before table", 10.0, 200.0, 12.0, FontWeight::Normal);
    span_before.reading_order = 0;

    // Text after the table (lower Y = later in reading order)
    let mut span_after = make_span("After table", 10.0, 20.0, 12.0, FontWeight::Normal);
    span_after.reading_order = 2;

    // Text inside table region whose text matches table cell content
    // (not an orphan — absorbed by the table rendering).
    let mut span_in_table = make_span("Val", 50.0, 70.0, 12.0, FontWeight::Normal);
    span_in_table.reading_order = 1;

    let mut table = Table::new();
    table.bbox = Some(Rect::new(10.0, 50.0, 200.0, 100.0));
    table.has_header = true;
    let mut header = TableRow::new(true);
    header.add_cell(TableCell::new("Col".to_string(), true));
    table.add_row(header);
    let mut data = TableRow::new(false);
    data.add_cell(TableCell::new("Val".to_string(), false));
    table.add_row(data);

    let result = converter
        .convert_with_tables(&[span_before, span_in_table, span_after], &[table], &config)
        .unwrap();

    assert!(
        result.contains("Before table"),
        "Should contain text before table"
    );
    assert!(result.contains("| Col |"), "Should contain table");
    assert!(
        result.contains("After table"),
        "Should contain text after table"
    );
}

#[test]
fn test_convert_with_tables_no_tables_is_same_as_convert() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    let spans = vec![make_span("Hello", 0.0, 100.0, 12.0, FontWeight::Normal)];

    let result_convert = converter.convert(&spans, &config).unwrap();
    let result_with_tables = converter.convert_with_tables(&spans, &[], &config).unwrap();

    assert_eq!(result_convert, result_with_tables);
}

#[test]
fn test_convert_with_tables_multiple_tables() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();

    let make_table = |x: f32, text: &str| -> Table {
        let mut t = Table::new();
        t.bbox = Some(Rect::new(x, 50.0, 100.0, 50.0));
        let mut row = TableRow::new(false);
        row.add_cell(TableCell::new(text.to_string(), false));
        t.add_row(row);
        t
    };

    let result = converter
        .convert_with_tables(
            &[],
            &[make_table(10.0, "T1"), make_table(200.0, "T2")],
            &config,
        )
        .unwrap();

    assert!(result.contains("| T1 |"), "Should contain first table");
    assert!(result.contains("| T2 |"), "Should contain second table");
}

// ============================================================================
// Issue #182: Bullet detection tests
// ============================================================================
