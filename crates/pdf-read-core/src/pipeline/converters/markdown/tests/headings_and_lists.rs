use super::*;

#[test]
fn test_tighten_list_items_collapses_blank_lines_between_markers() {
    // Loose list (blank lines between items) → tight list. CommonMark §5.3.
    let loose = "- a\n\n- b\n\n- c\n\nNext paragraph.";
    let tight = tighten_list_items(loose);
    assert_eq!(tight, "- a\n- b\n- c\n\nNext paragraph.");

    // Ordered list likewise.
    let loose_ol = "1. one\n\n2. two\n\nDone.";
    assert_eq!(tighten_list_items(loose_ol), "1. one\n2. two\n\nDone.");

    // Blank line separating a list from following prose is preserved.
    let mixed = "- only item\n\nA paragraph after the list.";
    assert_eq!(tighten_list_items(mixed), mixed);

    // Non-list content is untouched.
    let prose = "# Title\n\nPara one.\n\nPara two.";
    assert_eq!(tighten_list_items(prose), prose);
}

#[test]
fn test_is_md_list_item_line() {
    for yes in ["- x", "* y", "+ z", "1. a", "2) b", "  - indented"] {
        assert!(is_md_list_item_line(yes), "{yes:?} should be a list item");
    }
    for no in [
        "text",
        "-no space",
        "1.no space",
        "#- heading",
        "",
        "  plain",
    ] {
        assert!(
            !is_md_list_item_line(no),
            "{no:?} should NOT be a list item"
        );
    }
}

#[test]
fn test_bare_ordinal_suffix_is_not_a_heading() {
    // A stranded superscript ordinal must never be promoted to a heading.
    for ord in ["st", "nd", "rd", "th", "ST", "Th", " th "] {
        assert!(
            !MarkdownOutputConverter::is_valid_heading_text(ord),
            "{ord:?} must not be a valid heading"
        );
    }
    // Real (word-leading) headings stay valid.
    assert!(MarkdownOutputConverter::is_valid_heading_text(
        "Spring Equinox Gathering"
    ));
    assert!(MarkdownOutputConverter::is_valid_heading_text(
        "Eastern Apiary Update"
    ));
}

/// D1 RED — when the structure tree carries an explicit heading role
/// for a span (Word/Acrobat style: H1 → Span → MCR resolved by D8b),
/// the markdown converter must emit `# title` regardless of font-size
/// heuristics. Without this, every tagged Word document loses its
/// heading hierarchy because body and heading text are often the
/// same point size.
#[test]
fn test_struct_role_heading_emits_markdown_heading() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    let mut title = make_span("Document Title", 0.0, 100.0, 12.0, FontWeight::Normal);
    title.struct_role = Some(StructRole::Heading(1));
    let body = make_span("Body paragraph one.", 0.0, 80.0, 12.0, FontWeight::Normal);
    let result = converter.convert(&[title, body], &config).unwrap();
    assert!(
        result.contains("# Document Title"),
        "expected '# Document Title' in output, got:\n{}",
        result
    );
    assert!(result.contains("Body paragraph one."));
}

/// D1 RED — heading role precedence: even on the same font size as
/// body, Heading(2) must produce `## ...`. Mirrors the `nougat_011`
/// failure pattern where per-section headers are body-sized.
#[test]
fn test_struct_role_h2_overrides_font_size_heuristic() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    let mut h2 = make_span("Section Header", 0.0, 100.0, 11.0, FontWeight::Normal);
    h2.struct_role = Some(StructRole::Heading(2));
    let result = converter.convert(&[h2], &config).unwrap();
    assert!(
        result.starts_with("## "),
        "expected `## ` heading prefix, got:\n{}",
        result
    );
}

/// D3 unit — `is_ordered_list_marker` recognises common forms and
/// rejects look-alikes that are NOT lists (figure captions, years).
#[test]
fn test_is_ordered_list_marker_recognition() {
    // Recognised forms.
    assert_eq!(
        MarkdownOutputConverter::is_ordered_list_marker("1. Foo"),
        Some(1)
    );
    assert_eq!(
        MarkdownOutputConverter::is_ordered_list_marker("12. Foo"),
        Some(12)
    );
    assert_eq!(
        MarkdownOutputConverter::is_ordered_list_marker("a) Foo"),
        Some(1)
    );
    assert_eq!(
        MarkdownOutputConverter::is_ordered_list_marker("A. Foo"),
        Some(1)
    );
    assert_eq!(
        MarkdownOutputConverter::is_ordered_list_marker("iv. Foo"),
        Some(1)
    );
    // Conservative rejections so figure captions and years are not promoted.
    assert!(MarkdownOutputConverter::is_ordered_list_marker("1.1 Foo").is_none());
    assert!(MarkdownOutputConverter::is_ordered_list_marker("1986 was").is_none());
    assert!(MarkdownOutputConverter::is_ordered_list_marker("Item one").is_none());
}

/// D3 RED — three numbered items on consecutive lines must each
/// land on their own markdown line. Reproduces the nougat_037
/// "1. Treasurer ... 2. Safeguarding ... 3. Volunteering"
/// collapse pattern (those three were on different baselines but
/// joined by tight gap).
#[test]
fn test_numbered_list_consecutive_lines_separate() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    let s1 = make_span("1. Treasurer", 0.0, 100.0, 12.0, FontWeight::Normal);
    let s2 = make_span("2. Safeguarding", 0.0, 88.0, 12.0, FontWeight::Normal);
    let s3 = make_span("3. Volunteering", 0.0, 76.0, 12.0, FontWeight::Normal);
    let result = converter.convert(&[s1, s2, s3], &config).unwrap();
    for marker in ["1. Treasurer", "2. Safeguarding", "3. Volunteering"] {
        assert!(
            result.lines().any(|l| l.trim_start().starts_with(marker)),
            "expected line starting with `{}`, got:\n{}",
            marker,
            result
        );
    }
}

/// D4 RED — when an untagged paragraph is followed by a bullet list
/// with a small geometric gap, the list must still start on a new
/// line preceded by a blank line. Reproduces the `Intro sentence.•
/// First` glue pattern.
#[test]
fn test_bullet_after_paragraph_forces_break() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    // Tight gap: body 12pt, gap 4pt (well below 1.5×).
    let intro = make_span("Intro sentence.", 0.0, 100.0, 12.0, FontWeight::Normal);
    let b1 = make_span("• First item", 0.0, 88.0, 12.0, FontWeight::Normal);
    let b2 = make_span("• Second item", 0.0, 76.0, 12.0, FontWeight::Normal);
    let result = converter.convert(&[intro, b1, b2], &config).unwrap();
    assert!(
        result.contains("Intro sentence.\n\n- First item"),
        "expected blank line + bullet after intro, got:\n{}",
        result
    );
}

/// D1 coverage — every heading level H1..H6 from the structure tree
/// emits the matching markdown prefix. Lock-in for #377 word /
/// adobe-tagged docs whose body and heading text share a size.
#[test]
fn test_struct_role_emits_each_heading_level() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    for level in 1u8..=6 {
        let mut s = make_span(
            &format!("Title L{}", level),
            0.0,
            100.0,
            12.0,
            FontWeight::Normal,
        );
        s.struct_role = Some(StructRole::Heading(level));
        let body = make_span("body", 0.0, 80.0, 12.0, FontWeight::Normal);
        let result = converter.convert(&[s, body], &config).unwrap();
        let prefix = "#".repeat(level as usize);
        let expected = format!("{} Title L{}", prefix, level);
        assert!(
            result.contains(&expected),
            "expected `{}`, got:\n{}",
            expected,
            result
        );
    }
}

/// D1 coverage — out-of-range Heading level values are clamped to
/// the H1..H6 range. Defensive: a malformed structure tree
/// reporting Heading(0) or Heading(99) should not produce 0 or
/// 99 `#` characters.
#[test]
fn test_struct_role_heading_level_is_clamped() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    for raw_level in [0u8, 7, 99, 250] {
        let mut s = make_span("Edgy", 0.0, 100.0, 12.0, FontWeight::Normal);
        s.struct_role = Some(StructRole::Heading(raw_level));
        let result = converter.convert(&[s], &config).unwrap();
        // Find the prefix in the first line: count `#`s.
        let first_line = result.lines().next().unwrap_or("");
        let hash_count = first_line.chars().take_while(|c| *c == '#').count();
        assert!(
            (1..=6).contains(&hash_count),
            "raw_level {} produced {} `#`s in `{}`",
            raw_level,
            hash_count,
            first_line
        );
    }
}

/// A heading must not be glued to the first list item that follows it
/// (`## Highlights - Revenue…`).
#[test]
fn test_heading_not_glued_to_following_list() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    let mut h = make_span("Highlights", 0.0, 100.0, 12.0, FontWeight::Normal);
    h.struct_role = Some(StructRole::Heading(2));
    let mut a = make_span(
        "Revenue grew steadily.",
        0.0,
        80.0,
        12.0,
        FontWeight::Normal,
    );
    a.struct_role = Some(StructRole::ListItemBody);
    a.block_id = Some(1);
    let mut b = make_span("Costs remained flat.", 0.0, 64.0, 12.0, FontWeight::Normal);
    b.struct_role = Some(StructRole::ListItemBody);
    b.block_id = Some(2);
    let out = converter.convert(&[h, a, b], &config).unwrap();
    eprintln!("--- heading/list output ---\n{out}\n---");
    assert!(
        !out.contains("Highlights - "),
        "heading glued to list:\n{out}"
    );
    assert!(
        out.contains("- Revenue grew steadily."),
        "first item missing bullet:\n{out}"
    );
}

/// D1 coverage — every list-role variant (LI / Lbl / LBody) on a
/// span emits a `- ` bullet prefix. Lock-in against treating the
/// three roles inconsistently, which was the original
/// word365_structure regression.
#[test]
fn test_struct_role_all_list_variants_emit_bullets() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    for role in [
        StructRole::ListItem,
        StructRole::ListItemLabel,
        StructRole::ListItemBody,
    ] {
        let mut s = make_span("Item", 0.0, 100.0, 12.0, FontWeight::Normal);
        s.struct_role = Some(role);
        let result = converter.convert(&[s], &config).unwrap();
        assert!(
            result.lines().any(|l| l.starts_with("- ")),
            "role {:?} did not emit a bullet, got:\n{}",
            role,
            result
        );
    }
}

/// D1 coverage — heading immediately followed by a list-item must
/// transition cleanly: heading flushes, list emits bullet on a
/// fresh line. Cross-defect interaction guard.
#[test]
fn test_heading_then_list_item_transition() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    let mut h = make_span("Section", 0.0, 100.0, 12.0, FontWeight::Normal);
    h.struct_role = Some(StructRole::Heading(2));
    let mut li = make_span("First", 0.0, 80.0, 12.0, FontWeight::Normal);
    li.struct_role = Some(StructRole::ListItemBody);
    let result = converter.convert(&[h, li], &config).unwrap();
    assert!(result.contains("## Section"));
    assert!(result.contains("- First"));
    // The heading line must not also carry the bullet.
    assert!(
        !result.contains("## - "),
        "heading prefix and bullet must not co-occur, got:\n{}",
        result
    );
}

/// D3 coverage — extra whitelist + reject cases for ordered marker
/// detection. Locks the conservative behaviour that distinguishes
/// real lists from prose / numbers / captions.
#[test]
fn test_is_ordered_list_marker_extras() {
    // Recognised: trailing space required, both `.` and `)` close.
    assert_eq!(
        MarkdownOutputConverter::is_ordered_list_marker("99. Foo"),
        Some(99)
    );
    assert_eq!(
        MarkdownOutputConverter::is_ordered_list_marker("z) Last"),
        Some(1)
    );
    // Without the trailing space, not a list marker.
    assert!(MarkdownOutputConverter::is_ordered_list_marker("1.Foo").is_none());
    // Without a digit/letter prefix, not a list marker.
    assert!(MarkdownOutputConverter::is_ordered_list_marker(". Foo").is_none());
    assert!(MarkdownOutputConverter::is_ordered_list_marker(") Foo").is_none());
    // Empty / whitespace-only.
    assert!(MarkdownOutputConverter::is_ordered_list_marker("").is_none());
    assert!(MarkdownOutputConverter::is_ordered_list_marker("   ").is_none());
    // Looks like a list but is currency / unit / decimal.
    assert!(MarkdownOutputConverter::is_ordered_list_marker("$1. Total").is_none());
    assert!(MarkdownOutputConverter::is_ordered_list_marker("3.14 pi").is_none());
    // Long numeric (>3 digits) is not a marker (years, IDs).
    assert!(MarkdownOutputConverter::is_ordered_list_marker("2024. Year").is_none());
}

/// D2 RED — bold text only slightly larger than body must still be
/// detected as a heading. Many tagged-but-untyped corporate docs
/// (amt_handbook_sample, manuals) use bold + 1.05–1.1× body for
/// section headings without /H tags. Previous threshold was bold +
/// 1.10×.
#[test]
fn test_bold_slight_size_bump_is_heading() {
    let converter = MarkdownOutputConverter::new();
    let mut config = TextPipelineConfig::default();
    config.output.detect_headings = true;
    // Body at 11pt, "section header" bold at 11.55pt (1.05× body).
    let body_a = make_span("First body sentence.", 0.0, 100.0, 11.0, FontWeight::Normal);
    let body_b = make_span("Second body sentence.", 0.0, 88.0, 11.0, FontWeight::Normal);
    let head = make_span("Section Header", 0.0, 76.0, 11.55, FontWeight::Bold);
    let body_c = make_span("After-heading body.", 0.0, 64.0, 11.0, FontWeight::Normal);
    let result = converter
        .convert(&[body_a, body_b, head, body_c], &config)
        .unwrap();
    assert!(
        result.contains("### Section Header") || result.contains("#### Section Header"),
        "expected heading prefix on bold +5% line, got:\n{}",
        result
    );
}

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
