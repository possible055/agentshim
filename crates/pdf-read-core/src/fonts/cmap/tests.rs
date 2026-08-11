use super::parser::{extract_sections, parse_bfchar_line, parse_bfrange_line};
use super::*;

#[test]
fn test_parse_bfchar_single() {
    let data = b"beginbfchar\n<0041> <0041>\nendbfchar";
    let cmap = parse_tounicode_cmap(data).unwrap();
    assert_eq!(cmap.get(&0x41).as_deref(), Some("A"));
}

#[test]
fn test_parse_bfchar_multiple() {
    let data = b"beginbfchar\n<0041> <0041>\n<0042> <0042>\n<0043> <0043>\nendbfchar";
    let cmap = parse_tounicode_cmap(data).unwrap();
    assert_eq!(cmap.get(&0x41).as_deref(), Some("A"));
    assert_eq!(cmap.get(&0x42).as_deref(), Some("B"));
    assert_eq!(cmap.get(&0x43).as_deref(), Some("C"));
}

#[test]
fn test_large_bfrange_compresses_and_resolves() {
    // A 513-code contiguous range collapses into `ranges`, leaving `chars`
    // empty, and still resolves via computed range lookup.
    let data = b"beginbfrange\n<0100> <0300> <0500>\nendbfrange";
    let cmap = parse_tounicode_cmap(data).unwrap();
    assert!(
        !cmap.ranges.is_empty(),
        "large contiguous range should compress"
    );
    assert!(
        cmap.chars.is_empty(),
        "compressed codes should leave `chars`"
    );
    assert_eq!(cmap.get(&0x100).as_deref(), Some("\u{0500}"));
    assert_eq!(cmap.get(&0x300).as_deref(), Some("\u{0700}"));
    assert_eq!(cmap.get(&0x0FF), None);
    assert_eq!(cmap.get(&0x301), None);
}

#[test]
fn test_bfchar_override_survives_range_compression() {
    // A bfchar after a bfrange wins for that code (§9.10.3); compression must
    // not swallow it (it breaks contiguity and stays in `chars`).
    let data = b"beginbfrange\n<0100> <0300> <0500>\nendbfrange\n\
                     beginbfchar\n<0200> <0041>\nendbfchar";
    let cmap = parse_tounicode_cmap(data).unwrap();
    assert_eq!(
        cmap.get(&0x200).as_deref(),
        Some("A"),
        "later bfchar must win"
    );
    assert_eq!(cmap.get(&0x1FF).as_deref(), Some("\u{05FF}"));
    assert_eq!(cmap.get(&0x201).as_deref(), Some("\u{0601}"));
}

#[test]
fn test_parse_bfchar_non_ascii() {
    let data = b"beginbfchar\n<00E9> <00E9>\nendbfchar"; // é
    let cmap = parse_tounicode_cmap(data).unwrap();
    assert_eq!(cmap.get(&0xE9).as_deref(), Some("é"));
}

#[test]
fn test_parse_bfrange_simple() {
    let data = b"beginbfrange\n<0041> <0043> <0041>\nendbfrange";
    let cmap = parse_tounicode_cmap(data).unwrap();
    assert_eq!(cmap.get(&0x41).as_deref(), Some("A"));
    assert_eq!(cmap.get(&0x42).as_deref(), Some("B"));
    assert_eq!(cmap.get(&0x43).as_deref(), Some("C"));
}

#[test]
fn test_parse_bfrange_ascii_printable() {
    let data = b"beginbfrange\n<0020> <007E> <0020>\nendbfrange";
    let cmap = parse_tounicode_cmap(data).unwrap();

    // Check space
    assert_eq!(cmap.get(&0x20).as_deref(), Some(" "));
    // Check '0'
    assert_eq!(cmap.get(&0x30).as_deref(), Some("0"));
    // Check 'A'
    assert_eq!(cmap.get(&0x41).as_deref(), Some("A"));
    // Check 'z'
    assert_eq!(cmap.get(&0x7A).as_deref(), Some("z"));
    // Check '~'
    assert_eq!(cmap.get(&0x7E).as_deref(), Some("~"));
}

#[test]
fn test_parse_mixed_bfchar_bfrange() {
    let data =
        b"beginbfchar\n<0041> <0058>\nendbfchar\nbeginbfrange\n<0042> <0044> <0042>\nendbfrange";
    let cmap = parse_tounicode_cmap(data).unwrap();

    assert_eq!(cmap.get(&0x41).as_deref(), Some("X")); // Custom mapping
    assert_eq!(cmap.get(&0x42).as_deref(), Some("B")); // Range mapping
    assert_eq!(cmap.get(&0x43).as_deref(), Some("C"));
    assert_eq!(cmap.get(&0x44).as_deref(), Some("D"));
}

#[test]
fn test_parse_empty_cmap() {
    let data = b"";
    let cmap = parse_tounicode_cmap(data).unwrap();
    assert!(cmap.is_empty());
}

#[test]
fn test_parse_cmap_with_whitespace() {
    let data = b"beginbfchar\n  <0041>    <0041>  \n  <0042>  <0042>\nendbfchar";
    let cmap = parse_tounicode_cmap(data).unwrap();
    assert_eq!(cmap.get(&0x41).as_deref(), Some("A"));
    assert_eq!(cmap.get(&0x42).as_deref(), Some("B"));
}

#[test]
fn test_parse_bfchar_line() {
    assert_eq!(
        parse_bfchar_line("<0041> <0041>"),
        vec![(0x41, "A".to_string())]
    );
    assert_eq!(
        parse_bfchar_line("<00E9> <00E9>"),
        vec![(0xE9, "é".to_string())]
    );
    assert!(parse_bfchar_line("invalid line").is_empty());
}

#[test]
fn test_parse_bfchar_multiple_pairs_per_line() {
    let result = parse_bfchar_line("<01> <0041> <02> <0042> <03> <0043>");
    assert_eq!(result.len(), 3);
    assert_eq!(result[0], (0x01, "A".to_string()));
    assert_eq!(result[1], (0x02, "B".to_string()));
    assert_eq!(result[2], (0x03, "C".to_string()));
}

#[test]
fn test_parse_bfrange_line() {
    let result = parse_bfrange_line("<0041> <0043> <0041>").unwrap();
    assert_eq!(result.len(), 3);
    assert_eq!(result[0], (0x41, "A".to_string()));
    assert_eq!(result[1], (0x42, "B".to_string()));
    assert_eq!(result[2], (0x43, "C".to_string()));
}

#[test]
fn test_parse_bfrange_line_single_char() {
    let result = parse_bfrange_line("<0041> <0041> <0041>").unwrap();
    assert_eq!(result.len(), 1);
    assert_eq!(result[0], (0x41, "A".to_string()));
}

#[test]
fn test_parse_bfrange_line_invalid() {
    assert!(parse_bfrange_line("invalid").is_none());
}

#[test]
fn test_extract_sections() {
    let content =
        "before\nbeginbfchar\ndata1\nendbfchar\nmiddle\nbeginbfchar\ndata2\nendbfchar\nafter";
    let sections = extract_sections(content, "beginbfchar", "endbfchar");
    assert_eq!(sections.len(), 2);
    assert!(sections[0].contains("data1"));
    assert!(sections[1].contains("data2"));
}

#[test]
fn test_extract_sections_none() {
    let content = "no sections here";
    let sections = extract_sections(content, "beginbfchar", "endbfchar");
    assert_eq!(sections.len(), 0);
}

#[test]
fn test_parse_cid_to_unicode() {
    let data = b"beginbfchar\n<0041> <0041>\nendbfchar";
    let cmap = parse_cid_to_unicode(data).unwrap();
    assert_eq!(cmap.get(&0x41).as_deref(), Some("A"));
}

#[test]
fn test_parse_hex_case_insensitive() {
    let data = b"beginbfchar\n<00aB> <00Ab>\nendbfchar";
    let cmap = parse_tounicode_cmap(data).unwrap();
    assert_eq!(cmap.get(&0xAB).as_deref(), Some("«"));
}

#[test]
fn test_parse_multiple_sections() {
    let data = b"beginbfchar\n<0041> <0041>\nendbfchar\nbeginbfchar\n<0042> <0042>\nendbfchar";
    let cmap = parse_tounicode_cmap(data).unwrap();
    assert_eq!(cmap.len(), 2);
    assert_eq!(cmap.get(&0x41).as_deref(), Some("A"));
    assert_eq!(cmap.get(&0x42).as_deref(), Some("B"));
}

#[test]
fn test_parse_bfchar_ligature() {
    // Test single glyph to multiple characters (ligature expansion)
    let data = b"beginbfchar\n<000C> <00660069>\nendbfchar"; // fi ligature
    let cmap = parse_tounicode_cmap(data).unwrap();
    assert_eq!(cmap.get(&0x0C).as_deref(), Some("fi"));
}

#[test]
fn test_parse_bfchar_multiple_ligatures() {
    // Test multiple ligature mappings
    let data = b"beginbfchar\n<000B> <00660066>\n<000C> <00660069>\n<000D> <0066006C>\nendbfchar";
    let cmap = parse_tounicode_cmap(data).unwrap();
    assert_eq!(cmap.get(&0x0B).as_deref(), Some("ff")); // ff
    assert_eq!(cmap.get(&0x0C).as_deref(), Some("fi")); // fi
    assert_eq!(cmap.get(&0x0D).as_deref(), Some("fl")); // fl
}

#[test]
fn test_parse_bfrange_array_ligatures() {
    // Test bfrange with array format containing ligature mappings
    // Example from PDF spec: <005F> <0061> [<00660066> <00660069> <00660066006C>]
    let data = b"beginbfrange\n<005F> <0061> [<00660066> <00660069> <00660066006C>]\nendbfrange";
    let cmap = parse_tounicode_cmap(data).unwrap();
    assert_eq!(cmap.get(&0x5F).as_deref(), Some("ff")); // code 0x5F -> "ff"
    assert_eq!(cmap.get(&0x60).as_deref(), Some("fi")); // code 0x60 -> "fi"
    assert_eq!(cmap.get(&0x61).as_deref(), Some("ffl")); // code 0x61 -> "ffl"
}

#[test]
fn test_parse_bfrange_array_mixed() {
    // Test bfrange with array containing both single and multi-character mappings
    let data = b"beginbfrange\n<0010> <0012> [<0041> <00660069> <0043>]\nendbfrange";
    let cmap = parse_tounicode_cmap(data).unwrap();
    assert_eq!(cmap.get(&0x10).as_deref(), Some("A")); // code 0x10 -> "A"
    assert_eq!(cmap.get(&0x11).as_deref(), Some("fi")); // code 0x11 -> "fi"
    assert_eq!(cmap.get(&0x12).as_deref(), Some("C")); // code 0x12 -> "C"
}

#[test]
fn test_parse_zekat_cmap() {
    let cmap_data = r#"
/CIDInit /ProcSet findresource begin
19 dict begin
begincmap
/CIDSystemInfo
<< /Registry (Adobe)
/Ordering (UCS)
/Supplement 0
>> def
/CMapName /Adobe-Identity-UCS def
/CMapType 2 def
1 begincodespacerange
<0000> <FFFF>
endcodespacerange
1 beginbfrange
<0003> <0004> <0020>
endbfrange
3 beginbfchar
<000F> <002C>
<0011> <002E>
<0024> <0041>
endbfchar
1 beginbfrange
<0027> <0029> <0044>
endbfrange
2 beginbfchar
<002C> <0049>
<002E> <004B>
endbfchar
2 beginbfrange
<0030> <0032> <004D>
<0035> <0037> <0052>
endbfrange
2 beginbfchar
<0039> <0056>
<003D> <005A>
endbfchar
5 beginbfrange
<0044> <0048> <0061>
<004A> <004C> <0067>
<004E> <0053> <006B>
<0055> <0059> <0072>
<005C> <005D> <0079>
endbfrange
5 beginbfchar
<006B> <00E2>
<006F> <00E7>
<007C> <00F6>
<0081> <00FC>
<00AB> <2026>
endbfchar
1 beginbfrange
<00B3> <00B4> <201C>
endbfrange
4 beginbfchar
<00C6> <00C2>
<00D5> <0131>
<00F7> <011F>
<00FA> <015F>
endbfchar
endcmap
CMapName currentdict /CMap defineresource pop
end
end
"#
    .as_bytes();

    let cmap = parse_tounicode_cmap(cmap_data).expect("Failed to parse CMap");

    // ZEKAT check
    assert_eq!(cmap.get(&0x3D).as_deref(), Some("Z"));
    assert_eq!(cmap.get(&0x24).as_deref(), Some("A"));
    assert_eq!(cmap.get(&0xC6).as_deref(), Some("\u{00C2}")); // Â
}

/// `/WMode 1 def` on a CMap stream marks the font as vertical writing,
/// even when the CMap name does not advertise a `-V` suffix. This is the
/// authoritative signal per ISO 32000-1 §9.7.5.4 and is required for
/// embedded CMap streams used by tategaki layouts where the writer keeps
/// a horizontal-shaped CMap name but flips the writing mode internally.
#[test]
fn test_parse_wmode_vertical() {
    let data = b"\
/CIDInit /ProcSet findresource begin
12 dict begin
begincmap
/CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def
/CMapName /Adobe-Identity-UCS def
/CMapType 2 def
/WMode 1 def
1 begincodespacerange
<0000> <FFFF>
endcodespacerange
1 beginbfchar
<0041> <0041>
endbfchar
endcmap
CMapName currentdict /CMap defineresource pop
end
end
";
    let cmap = parse_tounicode_cmap(data).unwrap();
    assert_eq!(
        cmap.wmode, 1,
        "explicit /WMode 1 def must set vertical writing"
    );
    // Sanity: rest of the CMap still parses correctly.
    assert_eq!(cmap.get(&0x41).as_deref(), Some("A"));
    assert_eq!(cmap.code_width, 2);
}

/// Default WMode is `0` (horizontal) when the directive is absent. Most
/// ToUnicode CMaps for horizontal text omit `/WMode` entirely; this
/// guards the dominant code path.
#[test]
fn test_parse_wmode_default_horizontal() {
    let data = b"beginbfchar\n<0041> <0041>\nendbfchar";
    let cmap = parse_tounicode_cmap(data).unwrap();
    assert_eq!(cmap.wmode, 0, "missing /WMode must default to horizontal");
}

/// `/WMode 0 def` is a no-op but must be parsed without warning.
#[test]
fn test_parse_wmode_explicit_horizontal() {
    let data = b"\
begincmap
/WMode 0 def
1 begincodespacerange
<0000> <FFFF>
endcodespacerange
1 beginbfchar
<0041> <0041>
endbfchar
endcmap
";
    let cmap = parse_tounicode_cmap(data).unwrap();
    assert_eq!(cmap.wmode, 0);
}

/// M5: a `/WMode N def` directive that lives inside a PostScript
/// comment (`%` to end-of-line, §3.3.1) must NOT flip the writing
/// mode. Without comment-stripping, this commented-out producer
/// debug line would silently switch a horizontal CMap to vertical.
#[test]
fn test_parse_wmode_ignored_inside_postscript_comment() {
    // First-line commented-out directive — must be ignored.
    let data = b"\
begincmap
% /WMode 1 def
1 begincodespacerange
<0000> <FFFF>
endcodespacerange
1 beginbfchar
<0041> <0041>
endbfchar
endcmap
";
    let cmap = parse_tounicode_cmap(data).unwrap();
    assert_eq!(
        cmap.wmode, 0,
        "/WMode 1 def inside a PostScript comment must be ignored"
    );
}

/// M5 corollary: a legitimate `/WMode 1 def` on a later line is
/// still picked up even when an earlier line carries an unrelated
/// comment.
#[test]
fn test_parse_wmode_after_comment_still_seen() {
    let data = b"\
begincmap
% some prologue comment unrelated to wmode
/WMode 1 def
1 begincodespacerange
<0000> <FFFF>
endcodespacerange
1 beginbfchar
<0041> <0041>
endbfchar
endcmap
";
    let cmap = parse_tounicode_cmap(data).unwrap();
    assert_eq!(cmap.wmode, 1);
}

/// M6: a non-standard `/WMode 2 def` must NOT silently flip writing
/// mode; the spec only defines 0 and 1 (§9.7.5.4). Parser returns
/// None (callers fall back to horizontal default) and emits a warn
/// log so producer bugs are diagnosable.
#[test]
fn test_parse_wmode_non_standard_value_falls_back() {
    let data = b"\
begincmap
/WMode 2 def
1 begincodespacerange
<0000> <FFFF>
endcodespacerange
1 beginbfchar
<0041> <0041>
endbfchar
endcmap
";
    let cmap = parse_tounicode_cmap(data).unwrap();
    assert_eq!(
        cmap.wmode, 0,
        "/WMode 2 def is non-standard; parser must fall back to horizontal"
    );
}
