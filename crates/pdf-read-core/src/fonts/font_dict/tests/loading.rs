use super::*;

/// Build a minimal ToUnicode CMap stream that maps codes 0x0041–0x005A
/// (hex 2-byte keys) to U+0041–U+005A (A–Z).
fn make_tounicode_az() -> Vec<u8> {
    let stream = concat!(
        "/CIDInit /ProcSet findresource begin\n",
        "12 dict begin\n",
        "begincmap\n",
        "/CIDSystemInfo 3 dict dup begin\n",
        "  /Registry (Adobe) def\n",
        "  /Ordering (UCS) def\n",
        "  /Supplement 0 def\n",
        "end def\n",
        "/CMapName /Adobe-Identity-UCS def\n",
        "/CMapType 2 def\n",
        "1 begincodespacerange\n",
        "<0000> <FFFF>\n",
        "endcodespacerange\n",
        "26 beginbfchar\n",
        "<0041> <0041>\n", // A
        "<0042> <0042>\n",
        "<0043> <0043>\n",
        "<0044> <0044>\n",
        "<0045> <0045>\n",
        "<0046> <0046>\n",
        "<0047> <0047>\n",
        "<0048> <0048>\n",
        "<0049> <0049>\n",
        "<004A> <004A>\n",
        "<004B> <004B>\n",
        "<004C> <004C>\n",
        "<004D> <004D>\n",
        "<004E> <004E>\n",
        "<004F> <004F>\n",
        "<0050> <0050>\n",
        "<0051> <0051>\n",
        "<0052> <0052>\n",
        "<0053> <0053>\n",
        "<0054> <0054>\n",
        "<0055> <0055>\n",
        "<0056> <0056>\n",
        "<0057> <0057>\n",
        "<0058> <0058>\n",
        "<0059> <0059>\n",
        "<005A> <005A>\n", // Z
        "endbfchar\n",
        "endcmap\n",
        "CMapName currentdict /CMap defineresource pop\n",
        "end\n",
        "end\n",
    );
    stream.as_bytes().to_vec()
}

/// Build a minimal ToUnicode CMap that maps code 0x0001 to U+0007 (BEL).
fn make_tounicode_bel() -> Vec<u8> {
    let stream = concat!(
        "/CIDInit /ProcSet findresource begin\n",
        "12 dict begin\n",
        "begincmap\n",
        "/CIDSystemInfo 3 dict dup begin\n",
        "  /Registry (Adobe) def\n",
        "  /Ordering (UCS) def\n",
        "  /Supplement 0 def\n",
        "end def\n",
        "/CMapName /Test-BEL def\n",
        "/CMapType 2 def\n",
        "1 begincodespacerange\n",
        "<0000> <FFFF>\n",
        "endcodespacerange\n",
        "1 beginbfchar\n",
        "<0001> <0007>\n", // BEL control character
        "endbfchar\n",
        "endcmap\n",
        "CMapName currentdict /CMap defineresource pop\n",
        "end\n",
        "end\n",
    );
    stream.as_bytes().to_vec()
}

/// Construct a minimal Type0 FontInfo with the given ToUnicode stream and CIDSystemInfo.
fn make_type0_font(
    to_unicode_stream: Option<Vec<u8>>,
    encoding_name: &str,
    cid_system_info: Option<CIDSystemInfo>,
) -> FontInfo {
    FontInfo {
        base_font: "TestType0Font".to_string(),
        subtype: "Type0".to_string(),
        // Mirror the real parser (`parse_encoding`): a `/Identity-H` or
        // `/Identity-V` encoding name resolves to `Encoding::Identity`, not
        // `Encoding::Standard("Identity-H")` — production never produces the
        // latter for an Identity name, so tests must not either (#504).
        encoding: match encoding_name {
            "Identity-H" | "Identity-V" => Encoding::Identity,
            name => Encoding::Standard(name.to_string()),
        },
        to_unicode: to_unicode_stream.map(LazyCMap::new),
        font_weight: None,
        flags: None,
        stem_v: None,
        ascent: 0.95,
        descent: -0.35,
        embedded_font_data: None,
        truetype_cmap: std::sync::OnceLock::new(),
        embedded_glyph_names: std::sync::OnceLock::new(),
        is_truetype_font: false,
        widths: None,
        first_char: None,
        last_char: None,
        font_matrix_a: 0.001,
        default_width: 1000.0,
        cid_to_gid_map: None,
        cid_system_info,
        cid_font_type: None,
        cid_widths: None,
        cid_default_width: 1000.0,
        has_explicit_dw: false,
        cff_gid_map: None,
        multi_char_map: HashMap::new(),
        byte_to_char_table: std::sync::OnceLock::new(),
        type0_unicode_memo: std::sync::Arc::new(std::sync::Mutex::new(
            std::collections::HashMap::new(),
        )),
        byte_to_width_table: std::sync::OnceLock::new(),
        weight_memo: std::sync::OnceLock::new(),
        italic_memo: std::sync::OnceLock::new(),
        std14_memo: std::sync::OnceLock::new(),
        diff_glyph_names: std::collections::HashMap::new(),
        wmode: 0,
        cid_vertical_metrics: None,
        cid_default_vertical_metrics: VerticalMetrics::SPEC_DEFAULT,
        cjk_substitution: None,
    }
}

/// Fix A — ToUnicode present but code not covered → U+FFFD (no Priority-3 fallback).
///
/// A Type0 font with Adobe-GB1 ordering, a *non-Identity* predefined-CMap
/// encoding (`UniGB-UCS2-H` → `Encoding::Standard`), and a ToUnicode CMap
/// covering only A–Z. The Fix-A guard is deliberately scoped to
/// non-Identity Type0 fonts (Identity fonts map CID→Unicode directly
/// have a valid CMap-miss fallback), so the encoding here must be a real
/// predefined CMap — not Identity-H — for this guard to apply in
/// production. Querying code 0x0061 (not in the ToUnicode CMap) must
/// return U+FFFD, NOT the CJK character the Priority-3 predefined CMap
/// lookup would otherwise produce.
#[test]
fn test_fix_a_tounicode_present_miss_returns_fffd_not_cjk() {
    let cid_system_info = Some(CIDSystemInfo {
        registry: "Adobe".to_string(),
        ordering: "GB1".to_string(),
        supplement: 2,
    });
    let font = make_type0_font(Some(make_tounicode_az()), "UniGB-UCS2-H", cid_system_info);

    // Code 0x0061 ('a') is NOT in the ToUnicode CMap (which only covers A–Z).
    // The Priority-3 predefined CMap for Adobe-GB1 would map CID 97 to some
    // Latin character. With Fix A, the function must return U+FFFD instead.
    let result = font.char_to_unicode(0x0061);
    assert_eq!(
        result,
        Some("\u{FFFD}".to_string()),
        "Type0 font with ToUnicode present but missing code 0x61 must return U+FFFD, \
             not fall through to predefined CMap"
    );

    // Codes that ARE in the CMap (A–Z) must still work correctly.
    assert_eq!(font.char_to_unicode(0x0041), Some("A".to_string()));
    assert_eq!(font.char_to_unicode(0x005A), Some("Z".to_string()));
}

/// Fix A — ToUnicode absent, Priority-3 predefined CMap is triggered.
///
/// A Type0 font with Adobe-Japan1 ordering and NO ToUnicode CMap.
/// Querying CID 843 must return U+3042 (あ) via the predefined CMap.
///
/// `Identity-H` resolves to `Encoding::Identity` (as in production);
/// combined with a non-Identity CIDSystemInfo ordering (Japan1) and no
/// ToUnicode CMap, the lookup routes through the predefined-CMap path
/// (`lookup_predefined_cmap`) rather than treating the CID as a raw
/// Unicode code point.
#[test]
fn test_fix_a_no_tounicode_priority3_triggered() {
    let cid_system_info = Some(CIDSystemInfo {
        registry: "Adobe".to_string(),
        ordering: "Japan1".to_string(),
        supplement: 4,
    });
    // Identity-H with a non-Identity (Japan1) ordering: CIDs route through
    // lookup_predefined_cmap, NOT treated as raw Unicode code points.
    let font = make_type0_font(None, "Identity-H", cid_system_info);

    // CID 843 maps to U+3042 (あ) per the Adobe-Japan1 collection.
    let result = font.char_to_unicode(843);
    assert_eq!(
        result,
        Some("\u{3042}".to_string()),
        "Type0 font without ToUnicode must use predefined CMap for CID 843 → U+3042"
    );
}

/// Fix C — OOB CID guard: CID well beyond the Adobe-GB1 maximum → None.
///
/// lookup_predefined_cmap with an OOB CID must return None without panicking.
#[test]
fn test_fix_c_oob_cid_returns_none() {
    let cid_system_info = Some(CIDSystemInfo {
        registry: "Adobe".to_string(),
        ordering: "GB1".to_string(),
        supplement: 2,
    });
    // CID 99_999 is far beyond CID_MAX_GB1 (30_283).
    // The function takes u16, so we use the max u16 value (65535) which still
    // exceeds CID_MAX_GB1.
    let result = lookup_predefined_cmap("UniGB-UCS2-H", &cid_system_info, 65535);
    assert_eq!(
        result, None,
        "OOB CID (65535 > CID_MAX_GB1 30283) must return None"
    );

    // Same for Japan1.
    let cid_japan = Some(CIDSystemInfo {
        registry: "Adobe".to_string(),
        ordering: "Japan1".to_string(),
        supplement: 4,
    });
    let result_j = lookup_predefined_cmap("UniJIS-UCS2-H", &cid_japan, 65535);
    assert_eq!(
        result_j, None,
        "OOB CID (65535 > CID_MAX_JAPAN1 23059) must return None"
    );
}

/// Fix B — C0 control character filter: ToUnicode mapping to U+0007 (BEL) → U+FFFD.
///
/// A ToUnicode CMap that explicitly maps code 0x0001 to U+0007 (BEL).
/// The function must return U+FFFD, not the BEL character.
#[test]
fn test_fix_b_control_char_filter_returns_fffd() {
    let font = make_type0_font(Some(make_tounicode_bel()), "Identity-H", None);

    // Code 0x0001 maps to U+0007 (BEL) in the ToUnicode CMap.
    // Fix B must intercept this and return U+FFFD.
    let result = font.char_to_unicode(0x0001);
    assert_eq!(
        result,
        Some("\u{FFFD}".to_string()),
        "Code mapping to U+0007 (BEL) must be filtered to U+FFFD by Fix B"
    );
}

/// A ToUnicode CMap that maps only code 0x0041 → U+005A ('Z'); every other
/// code is absent.
fn make_tounicode_single_z() -> Vec<u8> {
    concat!(
        "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n",
        "/CIDSystemInfo 3 dict dup begin\n",
        "  /Registry (Adobe) def\n  /Ordering (UCS) def\n  /Supplement 0 def\nend def\n",
        "/CMapName /Test-Z def\n/CMapType 2 def\n",
        "1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n",
        "1 beginbfchar\n<0041> <005A>\nendbfchar\n",
        "endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n",
    )
    .as_bytes()
    .to_vec()
}

/// A structurally-valid `/ToUnicode` CMap with zero `bfchar`/`bfrange` entries:
/// present but maps nothing. Must count as *absent*.
fn make_tounicode_empty() -> Vec<u8> {
    concat!(
        "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n",
        "/CIDSystemInfo 3 dict dup begin\n",
        "  /Registry (Adobe) def\n  /Ordering (UCS) def\n  /Supplement 0 def\nend def\n",
        "/CMapName /Test-Empty def\n/CMapType 2 def\n",
        "1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n",
        "endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n",
    )
    .as_bytes()
    .to_vec()
}

/// With a present-but-incomplete `/ToUnicode` on an Identity-H Type0 font, a
/// drawn CID absent from it has no Unicode anywhere in the file, so it must
/// decode to U+FFFD rather than a numeric *guess* — the CID read as a code
/// point, or the GID via the standard glyph-name table → AGL. Both guess
/// paths are exercised: 0x0100 (CID-as-char) and 0x003A (gid 0x3A = "colon");
/// `cid_to_gid_map` is set so the gid→glyph-name path is actually reachable.
#[test]
fn test_type0_tounicode_gap_returns_fffd_not_guess() {
    let mut font = make_type0_font(Some(make_tounicode_single_z()), "Identity-H", None);
    font.cid_to_gid_map = Some(CIDToGIDMap::Identity);

    // Mapped code decodes via ToUnicode (proves the CMap is authoritative).
    assert_eq!(font.char_to_unicode(0x0041), Some("Z".to_string()));
    // Uncovered CIDs are unmapped, not guessed.
    assert_eq!(
        font.char_to_unicode(0x0100),
        Some("\u{FFFD}".to_string()),
        "uncovered CID must not be guessed as CID-as-Unicode"
    );
    assert_eq!(
        font.char_to_unicode(0x003A),
        Some("\u{FFFD}".to_string()),
        "uncovered CID must not be guessed via gid→glyph-name→AGL"
    );
}

/// Without a `/ToUnicode`, the CID-as-Unicode heuristic still applies — many
/// generators assign CID == Unicode — so this path must not regress to U+FFFD.
#[test]
fn test_type0_no_tounicode_keeps_cid_as_unicode() {
    let mut font = make_type0_font(None, "Identity-H", None);
    font.cid_to_gid_map = Some(CIDToGIDMap::Identity);
    assert_eq!(font.char_to_unicode(0x0100), Some("\u{0100}".to_string()));
}

/// For an Identity-ordered font with a present-but-incomplete `/ToUnicode`, an
/// uncovered CID decodes to U+FFFD (honest gap) rather than the CID-as-Unicode guess —
/// except whitespace (0x20 → space), which is retained so word boundaries survive.
#[test]
fn test_type0_identity_uncovered_cid_is_fffd_keeps_space() {
    let csi = CIDSystemInfo {
        registry: "Adobe".to_string(),
        ordering: "Identity".to_string(),
        supplement: 0,
    };
    let mut font = make_type0_font(Some(make_tounicode_single_z()), "Identity-H", Some(csi));
    font.cid_to_gid_map = Some(CIDToGIDMap::Identity);

    // Mapped code still resolves via ToUnicode.
    assert_eq!(
        font.char_to_unicode(0x0041),
        Some("Z".to_string()),
        "ToUnicode hit"
    );
    // Whitespace is retained even when uncovered (word boundaries survive).
    assert_eq!(
        font.char_to_unicode(0x0020),
        Some(" ".to_string()),
        "space retained"
    );
    // Any other uncovered CID → U+FFFD, not a CID-as-Unicode guess.
    assert_eq!(
        font.char_to_unicode(0x0043),
        Some("\u{FFFD}".to_string()),
        "uncovered non-space Identity CID must be U+FFFD"
    );
}

/// A present-but-*empty* `/ToUnicode` (0 bfchar/bfrange) maps nothing, so it must count
/// as absent and an Identity-ordered font must recover its text via CID-as-Unicode. The
/// `CIDToGIDMap` here remaps each letter to a low *punctuation* GID, so the GID→standard-
/// glyph-name→AGL guess (if it ran) would yield `J)'(i#`; CID-as-Unicode must win instead.
/// This is the faithful subset case the `CIDToGIDMap::Identity` variant can't reproduce.
#[test]
fn test_type0_identity_empty_tounicode_keeps_cid_as_unicode() {
    let csi = CIDSystemInfo {
        registry: "Adobe".to_string(),
        ordering: "Identity".to_string(),
        supplement: 1,
    };
    let mut font = make_type0_font(Some(make_tounicode_empty()), "Identity-H", Some(csi));

    // Each letter CID remaps to a punctuation GID (0x21..=0x26 → exclam/quotedbl/…),
    // which gid_to_standard_glyph_name → AGL would otherwise turn into a wrong char.
    let letters = [
        (0x004A, "J"),
        (0x0075, "u"),
        (0x0073, "s"),
        (0x0074, "t"),
        (0x0069, "i"),
        (0x006E, "n"),
    ];
    let mut gid_map = vec![0u16; 0x80];
    for (i, (cid, _)) in letters.iter().enumerate() {
        gid_map[*cid as usize] = 0x21 + i as u16;
    }
    font.cid_to_gid_map = Some(CIDToGIDMap::Explicit(gid_map));

    for (cid, ch) in letters {
        assert_eq!(
            font.char_to_unicode(cid),
            Some(ch.to_string()),
            "empty /ToUnicode + Identity ordering must use CID-as-Unicode for 0x{cid:04X}, \
                 not the GID→glyph-name guess"
        );
    }
}

/// #504: `make_type0_font` must mirror the real `parse_encoding`
/// mapping. A direct guard so a future revert of the helper is caught
/// tightly (the Fix-A/B tests above only assert it *indirectly* via
/// `char_to_unicode` outcomes).
#[test]
fn test_make_type0_font_encoding_matches_parser() {
    assert!(
            matches!(make_type0_font(None, "Identity-H", None).encoding, Encoding::Identity),
            "Identity-H must map to Encoding::Identity (production never yields Standard(\"Identity-H\"))"
        );
    assert!(
        matches!(
            make_type0_font(None, "Identity-V", None).encoding,
            Encoding::Identity
        ),
        "Identity-V must map to Encoding::Identity"
    );
    match make_type0_font(None, "WinAnsiEncoding", None).encoding {
        Encoding::Standard(ref n) => assert_eq!(n, "WinAnsiEncoding"),
        other => panic!("non-Identity name must stay Encoding::Standard, got {other:?}"),
    }
    match make_type0_font(None, "UniGB-UCS2-H", None).encoding {
        Encoding::Standard(ref n) => assert_eq!(n, "UniGB-UCS2-H"),
        other => panic!("predefined CMap name must be Encoding::Standard, got {other:?}"),
    }
}

/// Type0/CID fonts read ascent/descent from the CIDFont descendant's FontDescriptor
/// (§9.7.4 / Table 117), not from the Type0 wrapper (which has no top-level
/// /FontDescriptor). Verify that `FontInfo::from_dict` on a Type0 font with a
/// descendant FontDescriptor that carries Ascent=800 / Descent=-200 yields
/// ascent ≈ 0.8 and descent ≈ -0.2 (both normalised from 1/1000-em to fraction-of-em).
#[test]
fn test_type0_ascent_descent_from_descendant_descriptor() {
    // Build an inline CIDFont dictionary with a FontDescriptor containing Ascent/Descent.
    let mut desc: HashMap<String, Object> = HashMap::new();
    desc.insert(
        "Type".to_string(),
        Object::Name("FontDescriptor".to_string()),
    );
    desc.insert("Ascent".to_string(), Object::Integer(800));
    desc.insert("Descent".to_string(), Object::Integer(-200));

    // Build the CIDFont dictionary (inline, no object references needed).
    let mut cidfont: HashMap<String, Object> = HashMap::new();
    cidfont.insert("Type".to_string(), Object::Name("Font".to_string()));
    cidfont.insert(
        "Subtype".to_string(),
        Object::Name("CIDFontType0".to_string()),
    );
    cidfont.insert(
        "BaseFont".to_string(),
        Object::Name("TestCIDFont".to_string()),
    );
    cidfont.insert("DW".to_string(), Object::Integer(1000));
    cidfont.insert(
        "CIDSystemInfo".to_string(),
        Object::Dictionary({
            let mut si = HashMap::new();
            si.insert("Registry".to_string(), Object::String(b"Adobe".to_vec()));
            si.insert("Ordering".to_string(), Object::String(b"Identity".to_vec()));
            si.insert("Supplement".to_string(), Object::Integer(0));
            si
        }),
    );
    cidfont.insert("FontDescriptor".to_string(), Object::Dictionary(desc));

    // Wrap the CIDFont in the Type0 outer font dictionary.
    let mut type0: HashMap<String, Object> = HashMap::new();
    type0.insert("Type".to_string(), Object::Name("Font".to_string()));
    type0.insert("Subtype".to_string(), Object::Name("Type0".to_string()));
    type0.insert(
        "BaseFont".to_string(),
        Object::Name("TestType0Font".to_string()),
    );
    type0.insert(
        "Encoding".to_string(),
        Object::Name("Identity-H".to_string()),
    );
    type0.insert(
        "DescendantFonts".to_string(),
        Object::Array(vec![Object::Dictionary(cidfont)]),
    );

    let doc = minimal_pdf_doc();
    let font = FontInfo::from_dict(&Object::Dictionary(type0), &doc)
        .expect("Type0 font with inline descendant must parse");

    assert!(
        (font.ascent - 0.8).abs() < 1e-4,
        "Expected ascent ≈ 0.8 (800/1000), got {}",
        font.ascent
    );
    assert!(
        (font.descent - (-0.2)).abs() < 1e-4,
        "Expected descent ≈ -0.2 (-200/1000), got {}",
        font.descent
    );
}

// =========================================================================
// Item 1 (v0.3.58) — decimal-point / punctuation glyph recovery.
// =========================================================================

/// A minimal in-memory PDF so `parse_encoding` (which takes `&PdfDocument`)
/// can run in a unit test. The encoding dict and /Differences array below
/// use only inline objects, so the document is never actually dereferenced.
fn minimal_pdf_doc() -> crate::document::PdfDocument {
    let pdf = b"%PDF-1.4\n\
            1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n\
            2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n\
            3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>\nendobj\n\
            xref\n\
            0 4\n\
            0000000000 65535 f \n\
            0000000009 00000 n \n\
            0000000058 00000 n \n\
            0000000115 00000 n \n\
            trailer\n<< /Size 4 /Root 1 0 R >>\n\
            startxref\n197\n%%EOF\n";
    crate::document::PdfDocument::from_bytes(pdf.to_vec()).expect("minimal PDF should parse")
}

/// A synthetic CMMI-like encoding dict that parks `/period` at code 58.
fn cmmi_like_encoding_obj() -> Object {
    let mut enc: HashMap<String, Object> = HashMap::new();
    enc.insert(
        "Differences".to_string(),
        Object::Array(vec![
            Object::Integer(44),
            Object::Name("arrowhookleft".to_string()),
            Object::Integer(58),
            Object::Name("period".to_string()),
        ]),
    );
    Object::Dictionary(enc)
}

/// Task 1 verify: the /Differences glyph name survives parse time.
#[test]
fn test_diff_glyph_names_retains_period_for_code_58() {
    let doc = minimal_pdf_doc();
    let (_enc, _multi, diff_names) =
        FontInfo::parse_encoding(&cmmi_like_encoding_obj(), &doc, None).unwrap();
    assert_eq!(diff_names.get(&58).map(String::as_str), Some("period"));
    assert_eq!(
        diff_names.get(&44).map(String::as_str),
        Some("arrowhookleft")
    );
}

/// Task 2 verify: the closed-set AGL punctuation helper.
#[test]
fn test_punctuation_unicode_for_glyph_name_closed_set() {
    assert_eq!(punctuation_unicode_for_glyph_name("period"), Some("."));
    assert_eq!(punctuation_unicode_for_glyph_name("comma"), Some(","));
    assert_eq!(punctuation_unicode_for_glyph_name("hyphen"), Some("-"));
    assert_eq!(
        punctuation_unicode_for_glyph_name("minus"),
        Some("\u{2212}")
    );
    // Anything outside the closed set → None (no over-generalisation).
    assert_eq!(punctuation_unicode_for_glyph_name("colon"), None);
    assert_eq!(punctuation_unicode_for_glyph_name("logicalnot"), None);
    assert_eq!(punctuation_unicode_for_glyph_name("A"), None);
}

/// Task 2 verify: the non-sensible-symbol predicate.
#[test]
fn test_is_non_sensible_symbol() {
    // Wrong symbols that should trigger recovery.
    assert!(is_non_sensible_symbol("\u{00AC}")); // ¬ logicalnot
    assert!(is_non_sensible_symbol("\u{2192}")); // → rightwards arrow
    assert!(is_non_sensible_symbol("\u{2212}")); // − minus sign (math operator)
                                                 // Sensible punctuation / digits / letters → false.
    assert!(!is_non_sensible_symbol("."));
    assert!(!is_non_sensible_symbol(","));
    assert!(!is_non_sensible_symbol("-"));
    assert!(!is_non_sensible_symbol("5"));
    assert!(!is_non_sensible_symbol("A"));
    // Empty / multi-char never qualifies.
    assert!(!is_non_sensible_symbol(""));
    assert!(!is_non_sensible_symbol("ff"));
}

/// Build a CMMI-like simple font with the given ToUnicode CMap bytes and a
/// `/Differences 58 /period` side map (and matching Custom encoding entry).
fn cmmi_like_font(to_unicode: Option<&[u8]>, custom_char_for_58: char) -> FontInfo {
    let mut diff_glyph_names: HashMap<u8, String> = HashMap::new();
    diff_glyph_names.insert(58, "period".to_string());
    let mut custom_map: HashMap<u8, char> = HashMap::new();
    custom_map.insert(58, custom_char_for_58);
    make_font(|f| {
        f.base_font = "SQLQIW+CMMI10".to_string();
        f.subtype = "Type1".to_string();
        f.flags = Some(4); // symbolic
        f.encoding = Encoding::Custom(custom_map);
        f.to_unicode = to_unicode.map(|b| LazyCMap::new(b.to_vec()));
        f.diff_glyph_names = diff_glyph_names;
    })
}

/// Task 3 verify (Interception A): a non-sensible ToUnicode hit (U+00AC) for
/// a `/period`-named code is recovered to `.`.
#[test]
fn test_interception_a_tounicode_non_sensible_symbol_recovered() {
    // ToUnicode maps 0x3A → U+00AC (¬), a non-sensible symbol for /period.
    let cmap = b"beginbfchar\n<003A> <00AC>\nendbfchar";
    let font = cmmi_like_font(Some(cmap), '.');
    assert_eq!(font.char_to_unicode(0x3A), Some(".".to_string()));
}

/// Task 4 verify (Interception B): no ToUnicode, Custom encoding resolves 58
/// to a wrong symbol, but the /Differences /period name overrides to `.`.
#[test]
fn test_interception_b_custom_encoding_punctuation_override() {
    // Custom map deliberately resolves code 58 to ¬ (the wrong symbol);
    // diff_glyph_names[58] = "period" must win.
    let font = cmmi_like_font(None, '\u{00AC}');
    assert_eq!(font.char_to_unicode(0x3A), Some(".".to_string()));
}

/// Task 5 regression guard: correctly-mapped fonts and genuine symbols are
/// untouched by the punctuation-recovery interceptions.
#[test]
fn test_punctuation_recovery_regression_guard() {
    // (a) A correctly-mapped period via ToUnicode (0x2E → U+002E) with no
    //     special glyph name stays `.` — the hit is already sensible so
    //     Interception A never fires.
    let cmap_ok = b"beginbfchar\n<002E> <002E>\nendbfchar";
    let font_ok = make_font(|f| {
        f.to_unicode = Some(LazyCMap::new(cmap_ok.to_vec()));
    });
    assert_eq!(font_ok.char_to_unicode(0x2E), Some(".".to_string()));

    // (b) A genuine `logicalnot` glyph (¬) must stay ¬: its /Differences
    //     name is NOT in the punctuation closed set, so neither
    //     interception fires even though the resolved char is a symbol.
    let cmap_not = b"beginbfchar\n<0021> <00AC>\nendbfchar";
    let mut diff_glyph_names: HashMap<u8, String> = HashMap::new();
    diff_glyph_names.insert(0x21, "logicalnot".to_string());
    let font_not = make_font(|f| {
        f.base_font = "NSCCOE+txexs".to_string();
        f.flags = Some(4);
        f.to_unicode = Some(LazyCMap::new(cmap_not.to_vec()));
        f.diff_glyph_names = diff_glyph_names;
    });
    assert_eq!(font_not.char_to_unicode(0x21), Some("\u{00AC}".to_string()));
}

/// Build a synthetic single-page PDF whose object 4 is a Type0 font with
/// the given base name, /Encoding name, descendant /CIDSystemInfo
/// Ordering, and (optionally) an extra raw entry spliced into the
/// descendant's FontDescriptor (e.g. `/FontFile3 99 0 R` pointing at a
/// non-existent object to model a present-but-unextractable program).
fn build_predefined_cidfont_pdf(
    base_font: &str,
    encoding_name: &str,
    ordering: &str,
    descriptor_extra: &str,
) -> Vec<u8> {
    let mut pdf = Vec::new();
    pdf.extend_from_slice(b"%PDF-1.4\n");

    let o1 = pdf.len();
    pdf.extend_from_slice(b"1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n");
    let o2 = pdf.len();
    pdf.extend_from_slice(b"2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 >> endobj\n");
    let o3 = pdf.len();
    pdf.extend_from_slice(
        b"3 0 obj << /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
              /Resources << /Font << /F1 4 0 R >> >> >> endobj\n",
    );
    let o4 = pdf.len();
    pdf.extend_from_slice(
        format!(
            "4 0 obj << /Type /Font /Subtype /Type0 /BaseFont /{base_font} \
                 /Encoding /{encoding_name} /DescendantFonts [5 0 R] >> endobj\n"
        )
        .as_bytes(),
    );
    let o5 = pdf.len();
    pdf.extend_from_slice(
        format!(
            "5 0 obj << /Type /Font /Subtype /CIDFontType0 /BaseFont /{base_font} \
                 /CIDSystemInfo << /Registry (Adobe) /Ordering ({ordering}) /Supplement 6 >> \
                 /FontDescriptor 6 0 R /DW 1000 >> endobj\n"
        )
        .as_bytes(),
    );
    let o6 = pdf.len();
    pdf.extend_from_slice(
        format!(
            "6 0 obj << /Type /FontDescriptor /FontName /{base_font} /Flags 6 \
                 /FontBBox [-170 -331 1024 903] /ItalicAngle 0 /Ascent 723 \
                 /Descent -241 /CapHeight 709 /StemV 69 {descriptor_extra} >> endobj\n"
        )
        .as_bytes(),
    );

    let xref = pdf.len();
    pdf.extend_from_slice(b"xref\n0 7\n0000000000 65535 f \n");
    for off in [o1, o2, o3, o4, o5, o6] {
        pdf.extend_from_slice(format!("{:010} 00000 n \n", off).as_bytes());
    }
    pdf.extend_from_slice(
        format!(
            "trailer << /Size 7 /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
            xref
        )
        .as_bytes(),
    );
    pdf
}

/// Parse object 4 of a [`build_predefined_cidfont_pdf`] document through
/// the real `FontInfo::from_dict` path and return the resulting FontInfo.
fn parse_predefined_cidfont(
    base_font: &str,
    encoding_name: &str,
    ordering: &str,
    descriptor_extra: &str,
) -> FontInfo {
    let pdf = build_predefined_cidfont_pdf(base_font, encoding_name, ordering, descriptor_extra);
    let doc = crate::document::PdfDocument::from_bytes(pdf).expect("synthetic PDF must parse");
    let font_obj = doc
        .load_object(crate::object::ObjectRef::new(4, 0))
        .expect("load Type0 font dict");
    FontInfo::from_dict(&font_obj, &doc).expect("FontInfo::from_dict")
}

/// Control: an Identity-H predefined name with no font program is flagged
/// for substitution under the collection derived from the name.
#[test]
fn cjk_substitution_flags_identity_h_predefined_name() {
    let info = parse_predefined_cidfont("Ryumin-Light", "Identity-H", "Japan1", "");
    assert_eq!(
        info.cjk_substitution,
        Some(crate::fonts::predefined_cidfont::CharacterCollection::AdobeJapan1)
    );
}

/// A descriptor that *declares* a font program (here a /FontFile3 whose
/// target object doesn't exist, so extraction fails) must NOT be
/// substituted: the document intended to embed outlines and the decode
/// failure should surface as a warning, not be masked by a silent
/// sans-serif substitution.
#[test]
fn cjk_substitution_declined_when_font_program_key_present_but_unextractable() {
    let info =
        parse_predefined_cidfont("Ryumin-Light", "Identity-H", "Japan1", "/FontFile3 99 0 R");
    assert!(
        info.embedded_font_data.is_none(),
        "extraction must have failed"
    );
    assert_eq!(
        info.cjk_substitution, None,
        "substitution must not mask a failed embedded-font decode"
    );
}

/// Non-Identity predefined CMaps (90ms-RKSJ-H, GBK-EUC-H, …) carry raw
/// legacy multi-byte codes, not CIDs. Until a charcode→CID CMap pass is
/// wired, such fonts must not be substituted — interpreting a Shift-JIS
/// code as an Adobe-Japan1 CID paints wrong glyphs with no diagnostic.
#[test]
fn cjk_substitution_requires_identity_cmap_encoding() {
    let info = parse_predefined_cidfont("Ryumin-Light-90ms-RKSJ-H", "90ms-RKSJ-H", "Japan1", "");
    assert_eq!(
        info.cjk_substitution, None,
        "non-Identity CMap codes are not CIDs; substitution must decline"
    );
}

/// When the descendant's /CIDSystemInfo names a known collection that
/// disagrees with the one derived from the base-font name, the explicit
/// CIDSystemInfo wins — it is authoritative for CID semantics per
/// ISO 32000-1 §9.7.3.
#[test]
fn cjk_substitution_prefers_cid_system_info_ordering_over_name() {
    let info = parse_predefined_cidfont("Ryumin-Light", "Identity-H", "GB1", "");
    assert_eq!(
        info.cjk_substitution,
        Some(crate::fonts::predefined_cidfont::CharacterCollection::AdobeGB1),
        "explicit /CIDSystemInfo Ordering must override the name-derived collection"
    );
}

/// An Identity (or unknown) Ordering carries no collection semantics; the
/// name-derived collection remains the best available signal.
#[test]
fn cjk_substitution_keeps_name_collection_for_identity_ordering() {
    let info = parse_predefined_cidfont("Ryumin-Light", "Identity-H", "Identity", "");
    assert_eq!(
        info.cjk_substitution,
        Some(crate::fonts::predefined_cidfont::CharacterCollection::AdobeJapan1)
    );
}
