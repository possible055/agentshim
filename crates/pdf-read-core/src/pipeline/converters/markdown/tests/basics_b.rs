use super::*;

/// Wrapped list-item body that spans multiple visual lines (same
/// /LI struct elem, same block_id, same struct_role=ListItemBody)
/// must NOT emit a fresh `- ` bullet on the second visual line.
/// The break should fire on a list-item *transition*, not on the
/// mere presence of a list role.
#[test]
fn test_wrapped_list_item_body_does_not_emit_extra_bullet() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    let mut a = make_span(
        "First half of an item",
        0.0,
        100.0,
        12.0,
        FontWeight::Normal,
    );
    a.struct_role = Some(StructRole::ListItemBody);
    a.block_id = Some(7);
    let mut b = make_span(
        "that wraps to next line.",
        0.0,
        86.0,
        12.0,
        FontWeight::Normal,
    );
    b.struct_role = Some(StructRole::ListItemBody);
    b.block_id = Some(7);
    let result = converter.convert(&[a, b], &config).unwrap();
    let bullet_lines: Vec<&str> = result.lines().filter(|l| l.starts_with("- ")).collect();
    assert_eq!(
        bullet_lines.len(),
        1,
        "wrapped list item body must stay one bullet, got {} lines:\n{}",
        bullet_lines.len(),
        result
    );
}

/// D7-fix RED — Hebrew text already in logical Unicode order
/// (pdfium hebrew_mirrored.pdf shape) must NOT be reversed by
/// the markdown converter. Reproduces the v0.3.36 regression
/// where `בנימין` (logical) became `ןימינב` (reversed) after
/// the unconditional bidi reorder pass.
#[test]
fn test_logical_hebrew_passes_through_unchanged() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    let span = make_span("בנימין", 0.0, 100.0, 12.0, FontWeight::Normal);
    let result = converter.convert(&[span], &config).unwrap();
    assert!(
        result.contains("בנימין"),
        "Hebrew must survive intact; got: {:?}",
        result
    );
    assert!(
        !result.contains("ןימינב"),
        "must NOT contain reversed Hebrew; got: {:?}",
        result
    );
}

/// D7-fix RED — Arabic heading line must keep `#` at the start
/// after the converter runs. Reproduces the
/// pdfs_pdfjs/ArabicCIDTrueType.pdf regression where `# ﺔﻴﺑﺮﻌﻟا`
/// became `ﺔﻴﺑﺮﻌﻟا #` (hash moved to the end).
#[test]
fn test_arabic_heading_keeps_hash_at_start() {
    let converter = MarkdownOutputConverter::new();
    let mut config = TextPipelineConfig::default();
    config.output.detect_headings = true;
    let mut h = make_span("ﺔﻴﺑﺮﻌﻟا", 0.0, 100.0, 24.0, FontWeight::Bold);
    h.struct_role = Some(StructRole::Heading(1));
    let result = converter.convert(&[h], &config).unwrap();
    for line in result.lines() {
        if line.contains("ﺔﻴﺑﺮﻌﻟا") {
            assert!(
                line.trim_start().starts_with('#'),
                "heading line must start with `#`, got: {:?}",
                line
            );
        }
    }
}

/// D5d RED — IA_0047 reproducer. The struct tree emits the last
/// span of one column ("constitution" at x=976.7) immediately
/// followed by the first span of the next column ("Assailing" at
/// x=192.6) at the SAME baseline (y diff ≈ 1.5pt). A naive
/// converter joins these into "constitutionAssailing".
#[test]
fn test_backward_x_wrap_at_same_baseline_splits_paragraph() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    let mk = |t: &str, x: f32, y: f32| make_span(t, x, y, 12.0, FontWeight::Normal);
    // Mirrors IA_0047 spans 1677 → 1678 (column wrap on same line).
    let prev = mk("constitution", 976.7, 1013.2);
    let cur = mk("Assailing", 192.6, 1011.7);
    let result = converter.convert(&[prev, cur], &config).unwrap();
    assert!(
        !result.contains("constitutionAssailing"),
        "column wrap created concatenation, got:\n{}",
        result
    );
    // Both words must be present, on different paragraphs.
    let paras: Vec<&str> = result
        .split("\n\n")
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect();
    assert!(
        paras.len() >= 2,
        "expected ≥2 paragraphs from column wrap, got {} in:\n{}",
        paras.len(),
        result
    );
    assert!(result.contains("constitution"));
    assert!(result.contains("Assailing"));
}

/// D5d coverage — minor x backwards (≤ 2× font_size) is NOT a
/// column wrap. Could happen with tight kerning, italic
/// overhang, or the existing dedup code emitting near-duplicate
/// glyphs. Must NOT be promoted to a paragraph break.
#[test]
fn test_minor_x_backwards_within_tolerance_does_not_split() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    // 12pt font, x backs up by only 8pt (< 2 × 12 = 24pt).
    let prev = make_span("hello", 100.0, 100.0, 12.0, FontWeight::Normal);
    let cur = make_span("world", 92.0, 100.0, 12.0, FontWeight::Normal);
    let result = converter.convert(&[prev, cur], &config).unwrap();
    let paras: Vec<&str> = result
        .split("\n\n")
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect();
    assert_eq!(
        paras.len(),
        1,
        "minor backstep must stay on one paragraph: {:?}",
        result
    );
}

/// D5d coverage — the same backward-wrap detector fires when
/// block_ids are present (IA_0047 tagged paths) AND when no
/// block_ids are present (untagged multi-column docs).
#[test]
fn test_backward_x_wrap_works_with_or_without_block_id() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    for assign_block in [false, true] {
        let mut a = make_span("end of col1", 800.0, 100.0, 12.0, FontWeight::Normal);
        let mut b = make_span("Start of col2", 100.0, 100.0, 12.0, FontWeight::Normal);
        if assign_block {
            a.block_id = Some(1);
            b.block_id = Some(2);
        }
        let result = converter.convert(&[a, b], &config).unwrap();
        assert!(
            !result.contains("col1Start"),
            "block_id={}: column wrap concat in:\n{}",
            assign_block,
            result
        );
    }
}

/// D5d coverage — backward wrap on different baselines should
/// also produce a paragraph break (defensive: even if same_line
/// is false, a backwards x indicates layout boundary).
#[test]
fn test_backward_x_wrap_on_different_baseline() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    // Mimics column wrap with different baselines (column 1
    // bottom at y=200, column 2 top at y=600). is_paragraph_break
    // catches this via the gap heuristic, but we ensure the
    // backward-x detector does too as a safety net.
    let prev = make_span("col1 last", 800.0, 200.0, 12.0, FontWeight::Normal);
    let cur = make_span("Col2 first", 100.0, 600.0, 12.0, FontWeight::Normal);
    let result = converter.convert(&[prev, cur], &config).unwrap();
    assert!(!result.contains("lastCol2"));
}

/// D5d coverage — the exact pattern of all 5 regressions found in
/// IA_0047_20200204: lowercase end + uppercase start, same y,
/// negative x delta. Each must split into separate paragraphs.
#[test]
fn test_all_five_ia_0047_patterns_split() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    // Each tuple is (col1-end-word, col2-start-word, y, font_size).
    let patterns: &[(&str, &str, f32, f32)] = &[
        ("constitution", "Assailing", 1013.0, 12.0),
        ("harvesting", "Senator", 1162.0, 12.0),
        ("humoro", "Spartacus", 950.0, 11.0),
        ("posscssec", "France", 800.0, 12.0),
        ("should", "Satisfy", 600.0, 12.0),
    ];
    for (a, b, y, sz) in patterns {
        let prev = make_span(a, 800.0, *y, *sz, FontWeight::Normal);
        let cur = make_span(b, 150.0, *y - 1.0, *sz, FontWeight::Normal);
        let result = converter.convert(&[prev, cur], &config).unwrap();
        let joined = format!("{}{}", a, b);
        assert!(
            !result.contains(&joined),
            "pattern {:?}+{:?} created `{}` in:\n{}",
            a,
            b,
            joined,
            result
        );
    }
}

/// D5d coverage — column-wrap detector composes with D5b form
/// fix. A form heading split into pieces on the same baseline
/// (small forward gaps) still joins; only when the gap is
/// genuinely a column boundary (large forward OR backward) does
/// it split.
#[test]
fn test_column_wrap_does_not_break_form_heading_join() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    let mk = |t: &str, x: f32, bid: u32| {
        let mut s = make_span(t, x, 100.0, 18.0, FontWeight::Bold);
        s.struct_role = Some(StructRole::Heading(1));
        s.block_id = Some(bid);
        s
    };
    // All forward, small gaps.
    let spans = vec![
        mk("Form", 0.0, 1),
        mk("1040", 35.0, 2),
        mk("Title", 80.0, 3),
    ];
    let result = converter.convert(&spans, &config).unwrap();
    let heading_lines: Vec<&str> = result.lines().filter(|l| l.starts_with("# ")).collect();
    assert_eq!(
        heading_lines.len(),
        1,
        "form heading still joins: {}",
        result
    );
}

/// D5d unit — the helper itself. Property-style: matrix of
/// gap/baseline/font shapes covering positive, zero, small
/// negative, large negative, large positive.
#[test]
fn test_is_column_gap_matrix() {
    // (prev_x, prev_w, cur_x, font, expected)
    let cases: &[(f32, f32, f32, f32, bool)] = &[
        // Word gap inside a normal sentence: prev=Hello (50w) → cur="world".
        (100.0, 50.0, 154.0, 12.0, false),
        // Right at the 3× threshold: 36pt forward gap.
        (100.0, 50.0, 186.5, 12.0, true),
        // Far below threshold.
        (100.0, 50.0, 160.0, 12.0, false),
        // Backward 30pt at 12pt font (>2x = 24pt threshold).
        (200.0, 50.0, 100.0, 12.0, true),
        // Backward 8pt at 12pt font (under 24pt threshold).
        (100.0, 50.0, 92.0, 12.0, false),
        // Newspaper case: x=976→x=192 with 12pt font.
        (976.7, 37.8, 192.6, 12.0, true),
    ];
    for (px, pw, cx, font, expected) in cases {
        let prev = make_span("p", *px, 100.0, *font, FontWeight::Normal);
        let mut prev = prev;
        prev.span.bbox.width = *pw;
        let cur = make_span("c", *cx, 100.0, *font, FontWeight::Normal);
        let actual = is_column_gap(&prev, &cur);
        assert_eq!(
            actual, *expected,
            "(px={}, pw={}, cx={}, font={}) expected {} got {}",
            px, pw, cx, font, expected, actual
        );
    }
}

/// A backward-X transition is a column boundary ONLY when it is not an
/// ordinary within-column line wrap. A wrap drops by ~one line; a column
/// jump goes up, stays on the same baseline, or drops far more.
#[test]
fn test_is_column_gap_distinguishes_wrap_from_column_jump() {
    let font = 13.0;
    let mk = |x: f32, y: f32| {
        let mut s = make_span("w", x, y, font, FontWeight::Normal);
        s.span.bbox.width = 40.0;
        s
    };
    // Within-column wrap: X resets to the left margin, Y steps down one
    // line (~13.5pt). NOT a column boundary.
    assert!(!is_column_gap(&mk(285.0, 555.8), &mk(229.0, 542.2)));
    // Same-baseline column transition (end of col 1 → top-ish of col 2 at
    // the same Y, y_drop ≈ 0). IS a column boundary.
    assert!(is_column_gap(&mk(976.7, 1013.2), &mk(192.6, 1011.7)));
    // Column jump UP (next column resumes far above). IS a column boundary.
    assert!(is_column_gap(&mk(500.0, 100.0), &mk(72.0, 700.0)));
}

/// Right-to-left flow steps left on every word, including across the space
/// spans between words — none of those steps is a column boundary, or an
/// Arabic/Hebrew line shatters into one paragraph per word.
#[test]
fn test_is_column_gap_rtl_word_and_space_steps_are_not_columns() {
    let mk = |t: &str, x: f32| {
        let mut s = make_span(t, x, 700.0, 13.0, FontWeight::Normal);
        s.span.bbox.width = 30.0;
        s
    };
    // Hebrew word (right) → space → Hebrew word (further left), same line.
    let word_r = mk("\u{05D0}\u{05D1}", 300.0);
    let space = mk(" ", 290.0);
    let word_l = mk("\u{05D2}\u{05D3}", 250.0);
    assert!(
        !is_column_gap(&word_r, &space),
        "RTL word→space read as a column gap"
    );
    assert!(
        !is_column_gap(&space, &word_l),
        "RTL space→word read as a column gap"
    );
}

/// D5c RED — multi-column newspaper case. Two text spans on the
/// same baseline but in different columns (large horizontal gap
/// between the right edge of the previous span and the left edge
/// of the current one), with different structure-tree block_ids.
/// D5b would join them on one line and produce concatenated
/// gibberish like `andmight`. The column-gap detector must split
/// them into two paragraphs.
#[test]
fn test_column_gap_with_block_change_splits() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    // Column 1: "and" at x=0, width 30, baseline 100.
    // Column 2: "might" at x=180 (column gutter ≈ 150pt), baseline 100.
    // Body font 12pt, so the gap is well over 3× font_size.
    let mut col1 = make_span("and", 0.0, 100.0, 12.0, FontWeight::Normal);
    col1.block_id = Some(1);
    let mut col2 = make_span("might", 180.0, 100.0, 12.0, FontWeight::Normal);
    col2.block_id = Some(2);
    let result = converter.convert(&[col1, col2], &config).unwrap();
    // The two tokens must NOT be joined into `andmight`.
    assert!(
        !result.contains("andmight"),
        "column-gap join produced concatenated token, got:\n{}",
        result
    );
    // They must appear as separate words on separate lines or with
    // a paragraph break between them.
    assert!(result.contains("and"));
    assert!(result.contains("might"));
    // No `and might` glued onto one heading or paragraph either —
    // we want the two columns rendered as separate paragraphs.
    let paras: Vec<&str> = result
        .split("\n\n")
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect();
    assert!(
        paras.len() >= 2,
        "expected ≥2 paragraphs separated by column gap, got {} in:\n{}",
        paras.len(),
        result
    );
}

/// D5c coverage — same-baseline pieces of a tagged form heading
/// (small inline gap, different block_ids) must still JOIN even
/// after the column-gap detector. Regression guard for D5b.
#[test]
fn test_form_heading_inline_gap_still_joins() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    // `Form` ends at x≈30, `1040` starts at x≈40 — small inline
    // gap (≈10pt, well under 3× font_size = 54pt for 18pt heading).
    let mk = |t: &str, x: f32, bid: u32| {
        let mut s = make_span(t, x, 100.0, 18.0, FontWeight::Bold);
        s.struct_role = Some(StructRole::Heading(1));
        s.block_id = Some(bid);
        s
    };
    let spans = vec![
        mk("Form", 0.0, 1),
        mk("1040", 40.0, 2),
        mk("U.S.", 100.0, 3),
    ];
    let result = converter.convert(&spans, &config).unwrap();
    let heading_lines: Vec<&str> = result.lines().filter(|l| l.starts_with("# ")).collect();
    assert_eq!(
        heading_lines.len(),
        1,
        "small-gap form pieces must stay on one heading line, got:\n{}",
        result
    );
}

/// D5c coverage — boundary case: a moderate gap (e.g. 2× font
/// size, like a wide indent or cell separator) should NOT trigger
/// column split. Only truly large gaps (multi-column gutter)
/// trigger the break.
#[test]
fn test_moderate_gap_does_not_force_column_break() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    // Body 12pt, gap of 24pt (2× font_size) — wide indent but not
    // a column gutter.
    let mut a = make_span("First field", 0.0, 100.0, 12.0, FontWeight::Normal);
    a.block_id = Some(1);
    let mut b = make_span("Second field", 80.0, 100.0, 12.0, FontWeight::Normal);
    b.block_id = Some(2);
    // The gap from x=0+50 (text "First field" width=50 in make_span) to x=80 = 30pt = 2.5× font_size.
    // Just below the column-gap threshold (3× = 36pt).
    let result = converter.convert(&[a, b], &config).unwrap();
    let paras: Vec<&str> = result
        .split("\n\n")
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect();
    assert_eq!(
        paras.len(),
        1,
        "moderate gap (≈2.5× font) must keep content on one paragraph, got:\n{}",
        result
    );
}

/// D5c coverage — three columns at the same baseline with large
/// gaps must split into three paragraphs.
#[test]
fn test_three_column_layout_splits_into_three_paragraphs() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    let mk = |t: &str, x: f32, bid: u32| {
        let mut s = make_span(t, x, 100.0, 12.0, FontWeight::Normal);
        s.block_id = Some(bid);
        s
    };
    // Three 12pt-body columns at x=0, 200, 400 (gaps of ~150pt).
    let spans = vec![
        mk("col one", 0.0, 1),
        mk("col two", 200.0, 2),
        mk("col three", 400.0, 3),
    ];
    let result = converter.convert(&spans, &config).unwrap();
    let paras: Vec<&str> = result
        .split("\n\n")
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect();
    assert_eq!(
        paras.len(),
        3,
        "three columns must produce three paragraphs, got:\n{}",
        result
    );
}

/// D5c coverage — column-gap detector applies even when no
/// block_id is set (untagged document with multi-column layout).
/// Without this, untagged newspapers would also produce
/// `andmight`-style joins.
#[test]
fn test_column_gap_without_block_id_still_splits() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    // No block_id assigned (untagged).
    let a = make_span("left column.", 0.0, 100.0, 12.0, FontWeight::Normal);
    let b = make_span("right column.", 200.0, 100.0, 12.0, FontWeight::Normal);
    let result = converter.convert(&[a, b], &config).unwrap();
    // Pre-existing geometric heuristics should split too via the
    // group_id / has_horizontal_gap logic — verify the combined
    // result keeps the two columns as separate words at minimum.
    assert!(
        result.contains("left column") && result.contains("right column"),
        "both columns must surface, got:\n{}",
        result
    );
    // No concatenation across the gap.
    assert!(
        !result.contains("column.right"),
        "must not concatenate across column gap, got:\n{}",
        result
    );
}

/// D5b coverage — three-piece headings with a TINY (<1pt) y
/// jitter still considered same-line. Forms often have minute
/// baseline jitter due to font metric variation; the gate must be
/// tolerant.
#[test]
fn test_minor_baseline_jitter_still_joins() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    let mk = |t: &str, x: f32, y: f32, bid: u32| {
        let mut s = make_span(t, x, y, 18.0, FontWeight::Bold);
        s.struct_role = Some(StructRole::Heading(1));
        s.block_id = Some(bid);
        s
    };
    // y values jitter within 0.5pt — well within the same_line
    // threshold (font_size * 0.5 = 9pt for an 18pt heading).
    let spans = vec![
        mk("A", 0.0, 100.0, 1),
        mk("B", 30.0, 100.3, 2),
        mk("C", 60.0, 99.7, 3),
    ];
    let result = converter.convert(&spans, &config).unwrap();
    let heading_lines: Vec<&str> = result.lines().filter(|l| l.starts_with("# ")).collect();
    assert_eq!(
        heading_lines.len(),
        1,
        "tiny jitter must not split heading, got:\n{}",
        result
    );
}

/// D5b coverage — large baseline drop (well past same_line) DOES
/// split, even with same heading_level. Proves the gate isn't
/// over-suppressing.
#[test]
fn test_large_baseline_drop_still_splits_heading() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    let mk = |t: &str, y: f32, bid: u32| {
        let mut s = make_span(t, 0.0, y, 18.0, FontWeight::Bold);
        s.struct_role = Some(StructRole::Heading(1));
        s.block_id = Some(bid);
        s
    };
    // 30pt drop between baselines — far beyond `font_size * 0.5`.
    let spans = vec![mk("First Heading", 100.0, 1), mk("Second Heading", 70.0, 2)];
    let result = converter.convert(&spans, &config).unwrap();
    let heading_lines: Vec<&str> = result.lines().filter(|l| l.starts_with("# ")).collect();
    assert_eq!(
        heading_lines.len(),
        2,
        "two visually-separated headings must both surface, got:\n{}",
        result
    );
}

/// D5 RED — when adjacent spans carry different `block_id` from
/// the source PDF's structure tree, force a paragraph break even
/// when the geometric gap is too small for the
/// `paragraph_gap_ratio` heuristic. Reproduces the pdfa_049
/// pattern where two body-sized paragraphs sit ~14pt apart on a
/// 12pt body and our 1.5× heuristic (16.5pt threshold) merges
/// them. Tagged structure tree gives us authoritative paragraph
/// boundaries via `OrderedContent.block_id`.
#[test]
fn test_block_id_change_forces_paragraph_break() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    // Two paragraphs separated by 12pt (less than 1.5× line_height).
    let mut p1 = make_span(
        "Paragraph one body text.",
        0.0,
        100.0,
        12.0,
        FontWeight::Normal,
    );
    p1.block_id = Some(1);
    let mut p2 = make_span(
        "Paragraph two starts here.",
        0.0,
        88.0,
        12.0,
        FontWeight::Normal,
    );
    p2.block_id = Some(2);
    let result = converter.convert(&[p1, p2], &config).unwrap();
    assert!(
        result.contains("Paragraph one body text.\n\nParagraph two starts here."),
        "expected double newline between block_ids 1→2, got:\n{:?}",
        result
    );
}

/// D5 RED (negative) — same `block_id` keeps spans on the same
/// logical paragraph, even on different baselines (line wrap
/// inside one /P struct elem).
#[test]
fn test_same_block_id_keeps_paragraph_continuous() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    let mut l1 = make_span("first line", 0.0, 100.0, 12.0, FontWeight::Normal);
    l1.block_id = Some(7);
    let mut l2 = make_span("second line", 0.0, 88.0, 12.0, FontWeight::Normal);
    l2.block_id = Some(7);
    let result = converter.convert(&[l1, l2], &config).unwrap();
    // No blank line between them.
    assert!(
        !result.contains("\n\n"),
        "same block_id must not introduce paragraph break, got:\n{:?}",
        result
    );
}

/// D6 RED — a small superscript span (≤4 chars, fontSize < 0.7× the
/// preceding span) on a slightly raised baseline (PDF Ts/text-rise,
/// spec §9.4.3) must merge into the same logical line as the body
/// text instead of becoming its own paragraph. Reproduces the
/// `21st → "21" + bare "st"` corruption visible in nougat_002 and
/// the `23rd Street → "23" + "rd Street"` split visible in
/// nougat_011 line 43.
#[test]
fn test_superscript_text_rise_does_not_split_line() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    // Body baseline at y=100 with 11pt body font.
    let pre = make_span("On June 21", 0.0, 100.0, 11.0, FontWeight::Normal);
    // Superscript "st" raised ~2.5pt with 7pt font (smaller than body).
    let sup = make_span("st", 35.0, 102.5, 7.0, FontWeight::Normal);
    let post = make_span(" they met.", 42.0, 100.0, 11.0, FontWeight::Normal);
    let result = converter.convert(&[pre, sup, post], &config).unwrap();
    assert!(
        result.contains("21st they met"),
        "expected '21st they met' inline, got:\n{}",
        result
    );
    assert!(
        !result.lines().any(|l| l.trim() == "st"),
        "no bare 'st' line allowed, got:\n{}",
        result
    );
}

/// D1 RED — list item body MCRs must emit a bullet on a new line.
/// Reproduces the word365_structure / nougat_037 pattern where
/// consecutive items collapse into a single line because the
/// converter sees them as plain spans.
#[test]
fn test_struct_role_list_items_emit_bullets() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    let mut items = Vec::new();
    for (i, t) in ["Apple", "Banana", "Cherry"].iter().enumerate() {
        let mut s = make_span(t, 0.0, 100.0 - (i as f32 * 14.0), 12.0, FontWeight::Normal);
        s.struct_role = Some(StructRole::ListItemBody);
        items.push(s);
    }
    let result = converter.convert(&items, &config).unwrap();
    for t in ["- Apple", "- Banana", "- Cherry"] {
        assert!(
            result.contains(t),
            "expected `{}` line in output, got:\n{}",
            t,
            result
        );
    }
}
