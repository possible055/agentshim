use super::*;

#[test]
fn test_fence_monospace_blocks() {
    let n = MONO_SENTINEL;
    // Consecutive monospace paragraphs fuse into one fenced block, keeping
    // their internal line breaks; surrounding prose is untouched.
    let input = format!("Intro\n\n{n}line one{n}\n\n{n}line two{n}\n\nOutro");
    assert_eq!(
        fence_monospace_blocks(&input),
        "Intro\n\n```\nline one\nline two\n```\n\nOutro"
    );
    // A lone monospace paragraph still fences.
    assert_eq!(
        fence_monospace_blocks(&format!("{n}only{n}")),
        "```\nonly\n```"
    );
    // No sentinels → byte-identical.
    assert_eq!(fence_monospace_blocks("plain\n\ntext"), "plain\n\ntext");
    // A stray sentinel inside ordinary prose is stripped, not fenced.
    assert_eq!(fence_monospace_blocks(&format!("a{n}b")), "ab");
}

#[test]
fn test_push_plain_paragraph_marks_monospace() {
    let n = MONO_SENTINEL;
    let mut out = String::new();
    push_plain_paragraph(&mut out, "code", true);
    assert_eq!(out, format!("{n}code{n}\n\n"));
    let mut out2 = String::new();
    push_plain_paragraph(&mut out2, "prose", false);
    assert_eq!(out2, "prose\n\n");
}

/// D6 coverage — superscript inline merging across multiple
/// markers in the same line ("On the 1st, 2nd, and 3rd days").
/// Each "st"/"nd"/"rd" must inline-merge with its preceding
/// number.
#[test]
fn test_multiple_superscripts_one_line() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    // Body baseline at y=100 with 11pt; three superscripts raised
    // by 2.5pt with 7pt font.
    let parts: Vec<OrderedTextSpan> = vec![
        make_span("On the 1", 0.0, 100.0, 11.0, FontWeight::Normal),
        make_span("st", 25.0, 102.5, 7.0, FontWeight::Normal),
        make_span(", 2", 30.0, 100.0, 11.0, FontWeight::Normal),
        make_span("nd", 40.0, 102.5, 7.0, FontWeight::Normal),
        make_span(", and 3", 47.0, 100.0, 11.0, FontWeight::Normal),
        make_span("rd", 70.0, 102.5, 7.0, FontWeight::Normal),
        make_span(" days", 75.0, 100.0, 11.0, FontWeight::Normal),
    ];
    let result = converter.convert(&parts, &config).unwrap();
    // No bare superscript line.
    for sup in ["st", "nd", "rd"] {
        assert!(
            !result.lines().any(|l| l.trim() == sup),
            "bare `{}` line found in:\n{}",
            sup,
            result
        );
    }
    // Each composed token appears.
    for token in ["1st", "2nd", "3rd"] {
        assert!(
            result.contains(token),
            "expected `{}` in output, got:\n{}",
            token,
            result
        );
    }
}

/// `strip_inline_emphasis_in_rtl` must preserve non-ASCII (Arabic
/// / Hebrew) characters in the non-emphasis portion of an RTL
/// line. Earlier the function iterated the UTF-8 byte array and
/// pushed each byte as a Latin-1 char, corrupting `בנימין * world`
/// into `×<ctrl>×<ctrl>... * world`. The no-`*` short-circuit hid
/// the bug from earlier RTL tests.
#[test]
fn test_strip_inline_emphasis_preserves_rtl_chars_around_lone_asterisk() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    let span = make_span("בנימין * world", 0.0, 100.0, 12.0, FontWeight::Normal);
    let result = converter.convert(&[span], &config).unwrap();
    assert!(
        result.contains("בנימין"),
        "Hebrew letters lost — UTF-8 corruption: {:?}",
        result
    );
    assert!(
        !result
            .chars()
            .any(|c| (c as u32) == 0x91 || (c as u32) == 0xA0),
        "byte-as-char ghost characters present in: {:?}",
        result
    );
}

/// Arabic regression coverage — confirms `strip_inline_emphasis_in_rtl`
/// preserves Arabic across the no-`*`, single-`*`, paired-`*`,
/// and paired-`**` cases. Locks the Copilot-found UTF-8
/// corruption out for good across realistic shapes.
///
/// v0.3.55 (#537 follow-up): the markdown converter now also wraps
/// LTR runs inside RTL-dominant paragraphs with U+2066/U+2069
/// isolation markers. Substring assertions below strip those
/// markers before matching so this test continues to cover what
/// it was meant to cover — the emphasis-stripper's Arabic /
/// Hebrew preservation contract — independently of the new
/// bidi-isolation pass.
#[test]
fn test_arabic_strip_inline_emphasis_matrix() {
    let converter = MarkdownOutputConverter::new();
    let config = TextPipelineConfig::default();
    // Each tuple: (input span text, list of expected substrings).
    let cases: &[(&str, &[&str])] = &[
        // No `*` — short-circuit; must round-trip.
        ("اللغة العربية بسيطة", &["اللغة", "العربية", "بسيطة"]),
        // Hebrew with stray `*` (lone asterisk, no pair).
        ("בנימין * world", &["בנימין", "* world"]),
        // Arabic paragraph with `*emphasis*` around RTL token.
        ("مرحبا *عالم* اليوم", &["مرحبا", "عالم", "اليوم"]),
        // Arabic paragraph with `**bold**` around RTL token.
        ("مرحبا **عالم** اليوم", &["مرحبا", "عالم", "اليوم"]),
        // Mixed: emphasis around LTR (must keep markers) plus Arabic.
        ("مرحبا *Hello* اليوم", &["مرحبا", "*Hello*", "اليوم"]),
    ];
    for (input, expected_subs) in cases {
        let span = make_span(input, 0.0, 100.0, 12.0, FontWeight::Normal);
        let result = converter.convert(&[span], &config).unwrap();
        // Strip the v0.3.55 #537-follow-up bidi-isolation markers
        // (U+2066/U+2067/U+2068/U+2069) before substring checks —
        // they are correct, semantically additive, and orthogonal
        // to what this test exercises.
        let result_no_iso: String = result
            .chars()
            .filter(|c| !matches!(*c, '\u{2066}' | '\u{2067}' | '\u{2068}' | '\u{2069}'))
            .collect();
        for needle in *expected_subs {
            assert!(
                result_no_iso.contains(needle),
                "input {:?} → expected {:?} in output:\n{}",
                input,
                needle,
                result_no_iso
            );
        }
        // Ghost-byte check: no Latin-1 control chars from
        // mis-cast UTF-8 should appear. Run against the raw
        // result so any new ghost-byte regression still fires.
        assert!(
            !result.chars().any(|c| {
                let n = c as u32;
                (0x80..=0x9F).contains(&n) || n == 0xA0
            }),
            "input {:?} produced Latin-1 ghost chars in: {:?}",
            input,
            result
        );
    }
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
