use super::*;

// =========================================================================
// best_mapping_provenance — font-level Unicode-mapping capability (fact)
// =========================================================================

// The critical case: a Type0 / Identity-ordered subset with no ToUnicode
// and no embedded cmap severs every path to Unicode → Fallback.
#[test]
fn best_mapping_provenance_fallback_on_severed_identity_type0() {
    let f = make_font(|f| {
        f.subtype = "Type0".to_string();
        f.encoding = Encoding::Standard("Identity-H".to_string());
        f.cid_system_info = Some(CIDSystemInfo {
            registry: "Adobe".to_string(),
            ordering: "Identity".to_string(),
            supplement: 0,
        });
        f.to_unicode = None;
    });
    assert_eq!(
        f.best_mapping_provenance(),
        crate::fonts::MappingProvenance::Fallback
    );
}

#[test]
fn best_mapping_provenance_fallback_type0_without_cidsysteminfo() {
    let f = make_font(|f| {
        f.subtype = "Type0".to_string();
        f.encoding = Encoding::Standard("Identity-H".to_string());
        f.cid_system_info = None;
        f.to_unicode = None;
    });
    assert_eq!(
        f.best_mapping_provenance(),
        crate::fonts::MappingProvenance::Fallback
    );
}

// A known character collection (non-Identity ordering) → predefined CMap.
#[test]
fn best_mapping_provenance_predefined_for_known_collection() {
    let f = make_font(|f| {
        f.subtype = "Type0".to_string();
        f.cid_system_info = Some(CIDSystemInfo {
            registry: "Adobe".to_string(),
            ordering: "Japan1".to_string(),
            supplement: 6,
        });
    });
    assert_eq!(
        f.best_mapping_provenance(),
        crate::fonts::MappingProvenance::PredefinedCMap
    );
}

// A simple font resolves through its /Encoding → glyph name → AGL.
#[test]
fn best_mapping_provenance_encoding_for_simple_font() {
    let f = make_font(|_| {});
    assert_eq!(
        f.best_mapping_provenance(),
        crate::fonts::MappingProvenance::EncodingName
    );
}

// =========================================================================
// get_space_glyph_width — the space advance drives the geometric word-gap
// threshold, so it must be a REAL space advance, never an arbitrary glyph.
// Regression guard for #803 (justified TJ words glued together).
// =========================================================================

#[test]
fn space_width_identity_type0_ignores_cid32_glyph() {
    // #803: under Identity-H, character code 0x20 maps to CID 32 — an
    // arbitrary font glyph (real repro: TimesNewRomanPSMT reports 563 units
    // ≈ 0.56 em), NOT the space. Trusting it as the space advance inflated
    // the word-gap threshold (0.75 × 0.56 em) so far that real ~0.25 em
    // justified word gaps were suppressed and adjacent words glued together
    // ("All rights reserved" -> "Allrightsreserved"). The reference must
    // fall back to the 0.25 em (250-unit) typographic default instead.
    let mut cid_widths = HashMap::new();
    cid_widths.insert(0x20_u16, 563.0);
    let font = make_font(|f| {
        f.subtype = "Type0".to_string();
        f.encoding = Encoding::Identity;
        f.cid_widths = Some(cid_widths);
    });
    assert_eq!(
        font.get_space_glyph_width(),
        250.0,
        "Identity Type0 must not treat the CID-32 glyph width as the space advance"
    );
}

#[test]
fn space_width_identity_type0_without_cid32_defaults() {
    // Identity Type0 with no /W entry for CID 32 also defaults to 0.25 em.
    let font = make_font(|f| {
        f.subtype = "Type0".to_string();
        f.encoding = Encoding::Identity;
        f.cid_widths = Some(HashMap::new());
    });
    assert_eq!(font.get_space_glyph_width(), 250.0);
}

#[test]
fn space_width_non_identity_type0_trusts_explicit_space_cid() {
    // A non-Identity predefined CMap can genuinely place the space at code
    // 0x20, so an explicit /W entry there is a real space advance and is
    // kept — only Identity encoding remaps 0x20 to an arbitrary CID.
    let mut cid_widths = HashMap::new();
    cid_widths.insert(0x20_u16, 280.0);
    let font = make_font(|f| {
        f.subtype = "Type0".to_string();
        f.encoding = Encoding::Standard("Predefined-CMap".to_string());
        f.cid_widths = Some(cid_widths);
    });
    assert_eq!(font.get_space_glyph_width(), 280.0);
}

#[test]
fn space_width_simple_font_uses_explicit_widths_space() {
    // A simple font that declares 0x20 in /Widths (FirstChar covers code 32)
    // uses that real space advance unchanged.
    let font = make_font(|f| {
        f.subtype = "Type1".to_string();
        f.first_char = Some(32);
        // index 0 = code 32 (space) = 260 units.
        f.widths = Some(vec![260.0, 500.0, 500.0]);
    });
    assert_eq!(font.get_space_glyph_width(), 260.0);
}

// =========================================================================
// parse_cid_widths — unit tests for the /W array parser
// =========================================================================

#[test]
fn test_parse_cid_widths_array_format() {
    // Format: c [w1 w2 ... wn]
    let mut dict: HashMap<String, Object> = HashMap::new();
    dict.insert(
        "W".to_string(),
        Object::Array(vec![
            Object::Integer(10), // start CID
            Object::Array(vec![
                Object::Integer(500),
                Object::Integer(600),
                Object::Integer(700),
            ]),
        ]),
    );
    let widths = FontInfo::parse_cid_widths(&dict, "Test").unwrap();
    assert_eq!(widths.get(&10), Some(&500.0));
    assert_eq!(widths.get(&11), Some(&600.0));
    assert_eq!(widths.get(&12), Some(&700.0));
    assert_eq!(widths.get(&13), None);
}

#[test]
fn test_parse_cid_widths_range_format() {
    // Format: cfirst clast w
    let mut dict: HashMap<String, Object> = HashMap::new();
    dict.insert(
        "W".to_string(),
        Object::Array(vec![
            Object::Integer(100),
            Object::Integer(105),
            Object::Integer(300),
        ]),
    );
    let widths = FontInfo::parse_cid_widths(&dict, "Test").unwrap();
    for cid in 100..=105 {
        assert_eq!(widths.get(&cid), Some(&300.0), "CID {} should be 300", cid);
    }
    assert_eq!(widths.get(&106), None);
}

#[test]
fn test_parse_cid_widths_mixed_formats() {
    // Mix array-format and range-format in one /W array
    let mut dict: HashMap<String, Object> = HashMap::new();
    dict.insert(
        "W".to_string(),
        Object::Array(vec![
            // Array format
            Object::Integer(1),
            Object::Array(vec![Object::Integer(200), Object::Integer(300)]),
            // Range format
            Object::Integer(50),
            Object::Integer(52),
            Object::Integer(400),
        ]),
    );
    let widths = FontInfo::parse_cid_widths(&dict, "Test").unwrap();
    assert_eq!(widths.get(&1), Some(&200.0));
    assert_eq!(widths.get(&2), Some(&300.0));
    assert_eq!(widths.get(&50), Some(&400.0));
    assert_eq!(widths.get(&51), Some(&400.0));
    assert_eq!(widths.get(&52), Some(&400.0));
}

#[test]
fn test_parse_cid_widths_real_values() {
    // Widths specified as Real (float) values
    let mut dict: HashMap<String, Object> = HashMap::new();
    dict.insert(
        "W".to_string(),
        Object::Array(vec![
            Object::Integer(5),
            Object::Array(vec![Object::Real(123.5)]),
        ]),
    );
    let widths = FontInfo::parse_cid_widths(&dict, "Test").unwrap();
    assert_eq!(widths.get(&5), Some(&123.5));
}

#[test]
fn test_parse_cid_widths_empty_array() {
    let mut dict: HashMap<String, Object> = HashMap::new();
    dict.insert("W".to_string(), Object::Array(vec![]));
    assert!(FontInfo::parse_cid_widths(&dict, "Test").is_none());
}

#[test]
fn test_parse_cid_widths_missing_w() {
    let dict: HashMap<String, Object> = HashMap::new();
    assert!(FontInfo::parse_cid_widths(&dict, "Test").is_none());
}

#[test]
fn test_parse_cid_widths_non_integer_start() {
    // First element is not an integer — should skip
    let mut dict: HashMap<String, Object> = HashMap::new();
    dict.insert(
        "W".to_string(),
        Object::Array(vec![
            Object::Name("bad".to_string()),
            Object::Integer(10),
            Object::Array(vec![Object::Integer(500)]),
        ]),
    );
    let widths = FontInfo::parse_cid_widths(&dict, "Test").unwrap();
    assert_eq!(widths.get(&10), Some(&500.0));
}

#[test]
fn test_parse_cid_widths_truncated_range() {
    // Range format with missing width — should just stop
    let mut dict: HashMap<String, Object> = HashMap::new();
    dict.insert(
        "W".to_string(),
        Object::Array(vec![
            Object::Integer(10),
            Object::Integer(15),
            // missing width
        ]),
    );
    assert!(FontInfo::parse_cid_widths(&dict, "Test").is_none());
}

#[test]
fn test_parse_cid_widths_unexpected_second_element() {
    // Second element after CID is neither Array nor Integer
    let mut dict: HashMap<String, Object> = HashMap::new();
    dict.insert(
        "W".to_string(),
        Object::Array(vec![Object::Integer(10), Object::Name("bad".to_string())]),
    );
    // Should produce empty widths
    assert!(FontInfo::parse_cid_widths(&dict, "Test").is_none());
}

#[test]
fn test_parse_cid_widths_range_with_bad_width() {
    // Range format where the width value is not a number
    let mut dict: HashMap<String, Object> = HashMap::new();
    dict.insert(
        "W".to_string(),
        Object::Array(vec![
            Object::Integer(1),
            Object::Integer(3),
            Object::Name("notanumber".to_string()),
        ]),
    );
    // Bad width for range, should skip and produce no widths
    assert!(FontInfo::parse_cid_widths(&dict, "Test").is_none());
}

#[test]
fn test_parse_cid_widths_range_with_real_width() {
    let mut dict: HashMap<String, Object> = HashMap::new();
    dict.insert(
        "W".to_string(),
        Object::Array(vec![
            Object::Integer(10),
            Object::Integer(12),
            Object::Real(750.5),
        ]),
    );
    let widths = FontInfo::parse_cid_widths(&dict, "Test").unwrap();
    assert_eq!(widths.get(&10), Some(&750.5));
    assert_eq!(widths.get(&11), Some(&750.5));
    assert_eq!(widths.get(&12), Some(&750.5));
}

// =========================================================================
// parse_cid_vertical_metrics + parse_dw2 — /W2 and /DW2 (vertical writing)
// =========================================================================

/// `/W2` Form A: `c [ w1y v_x v_y w1y v_x v_y … ]` assigns successive
/// triples to CIDs `c`, `c+1`, `c+2`, … Drives per-CID lookups for
/// vertical advance and vertical-origin offset on tategaki layouts.
#[test]
fn test_parse_w2_explicit_array_form() {
    let mut dict: HashMap<String, Object> = HashMap::new();
    dict.insert(
        "W2".to_string(),
        Object::Array(vec![
            Object::Integer(10),
            Object::Array(vec![
                // CID 10: w1y=-880 v_x=500 v_y=900
                Object::Integer(-880),
                Object::Integer(500),
                Object::Integer(900),
                // CID 11: w1y=-1000 v_x=520 v_y=850
                Object::Integer(-1000),
                Object::Integer(520),
                Object::Integer(850),
            ]),
        ]),
    );
    let metrics = FontInfo::parse_cid_vertical_metrics(&dict, "Test").unwrap();
    assert_eq!(
        metrics.get(&10),
        Some(&VerticalMetrics {
            w1y: -880.0,
            v_x: 500.0,
            v_y: 900.0
        })
    );
    assert_eq!(
        metrics.get(&11),
        Some(&VerticalMetrics {
            w1y: -1000.0,
            v_x: 520.0,
            v_y: 850.0
        })
    );
    assert_eq!(metrics.get(&12), None);
}

/// `/W2` Form B: `c_first c_last w1y v_x v_y` assigns the same metrics
/// to every CID in the inclusive range.
#[test]
fn test_parse_w2_range_form() {
    let mut dict: HashMap<String, Object> = HashMap::new();
    dict.insert(
        "W2".to_string(),
        Object::Array(vec![
            Object::Integer(100),
            Object::Integer(102),
            Object::Integer(-1000),
            Object::Integer(500),
            Object::Integer(880),
        ]),
    );
    let metrics = FontInfo::parse_cid_vertical_metrics(&dict, "Test").unwrap();
    let expected = VerticalMetrics {
        w1y: -1000.0,
        v_x: 500.0,
        v_y: 880.0,
    };
    assert_eq!(metrics.get(&100), Some(&expected));
    assert_eq!(metrics.get(&101), Some(&expected));
    assert_eq!(metrics.get(&102), Some(&expected));
    assert_eq!(metrics.get(&103), None);
    assert_eq!(metrics.get(&99), None);
}

/// `/W2` Form A and Form B can be intermixed in a single array. Real
/// CIDFonts use this routinely — explicit triples for outliers and
/// ranges for runs of full-width CJK glyphs.
#[test]
fn test_parse_w2_mixed_forms() {
    let mut dict: HashMap<String, Object> = HashMap::new();
    dict.insert(
        "W2".to_string(),
        Object::Array(vec![
            // Form A: CID 5 explicit triple
            Object::Integer(5),
            Object::Array(vec![
                Object::Integer(-900),
                Object::Integer(490),
                Object::Integer(870),
            ]),
            // Form B: CIDs 200..=201
            Object::Integer(200),
            Object::Integer(201),
            Object::Integer(-1000),
            Object::Integer(500),
            Object::Integer(880),
        ]),
    );
    let metrics = FontInfo::parse_cid_vertical_metrics(&dict, "Test").unwrap();
    assert_eq!(
        metrics.get(&5),
        Some(&VerticalMetrics {
            w1y: -900.0,
            v_x: 490.0,
            v_y: 870.0
        })
    );
    let range_default = VerticalMetrics {
        w1y: -1000.0,
        v_x: 500.0,
        v_y: 880.0,
    };
    assert_eq!(metrics.get(&200), Some(&range_default));
    assert_eq!(metrics.get(&201), Some(&range_default));
    assert_eq!(metrics.get(&202), None);
}

/// Missing `/W2` ⇒ `None`. Horizontal-only fonts must skip the HashMap
/// allocation so they pay no per-glyph lookup cost in the hot path.
#[test]
fn test_parse_w2_missing_returns_none() {
    let dict: HashMap<String, Object> = HashMap::new();
    assert!(FontInfo::parse_cid_vertical_metrics(&dict, "Test").is_none());
}

/// Empty `/W2` array ⇒ `None`.
#[test]
fn test_parse_w2_empty_returns_none() {
    let mut dict: HashMap<String, Object> = HashMap::new();
    dict.insert("W2".to_string(), Object::Array(vec![]));
    assert!(FontInfo::parse_cid_vertical_metrics(&dict, "Test").is_none());
}

/// `/W2` accepts real-valued metrics (some writers use floats for
/// fine-tuned vertical adjustments).
#[test]
fn test_parse_w2_real_values() {
    let mut dict: HashMap<String, Object> = HashMap::new();
    dict.insert(
        "W2".to_string(),
        Object::Array(vec![
            Object::Integer(1),
            Object::Integer(1),
            Object::Real(-987.5),
            Object::Real(501.25),
            Object::Real(879.75),
        ]),
    );
    let metrics = FontInfo::parse_cid_vertical_metrics(&dict, "Test").unwrap();
    assert_eq!(
        metrics.get(&1),
        Some(&VerticalMetrics {
            w1y: -987.5,
            v_x: 501.25,
            v_y: 879.75
        })
    );
}

/// `/W2` Form A with a malformed inner triple must not desynchronise
/// the CID assignment of subsequent triples. The original
/// implementation advanced `j` by 1 on a non-numeric element without
/// touching `emitted`, so every following triple was shifted up by
/// one CID. Spec stance: a triple is atomic — drop the whole triple
/// (advance `j` by 3 and `emitted` by 1) so the CID alignment of the
/// rest of the inner array is preserved.
#[test]
fn test_parse_w2_form_a_skips_malformed_triple_without_desync() {
    let mut dict: HashMap<String, Object> = HashMap::new();
    // CID 10 is intentionally malformed (a name where w1y should be).
    // CID 11 must remain aligned to its proper triple, not slide into
    // CID 10's slot.
    dict.insert(
        "W2".to_string(),
        Object::Array(vec![
            Object::Integer(10),
            Object::Array(vec![
                // CID 10: malformed (name instead of number).
                Object::Name("Bogus".to_string()),
                Object::Integer(500),
                Object::Integer(880),
                // CID 11: well-formed (-1000, 500, 880).
                Object::Integer(-1000),
                Object::Integer(500),
                Object::Integer(880),
            ]),
        ]),
    );
    let metrics = FontInfo::parse_cid_vertical_metrics(&dict, "Test").unwrap();
    // CID 10 was malformed: must NOT carry the metrics that belong to CID 11.
    assert!(
        !metrics.contains_key(&10),
        "malformed CID 10 must not appear in metrics; got {:?}",
        metrics.get(&10)
    );
    // CID 11 must carry its own metrics — not collapsed onto CID 10 or
    // shifted into a different CID slot.
    assert_eq!(
        metrics.get(&11),
        Some(&VerticalMetrics {
            w1y: -1000.0,
            v_x: 500.0,
            v_y: 880.0
        })
    );
}

/// `/W2` Form B near the top of the u16 range must not silently
/// collapse every overflowing CID onto u16::MAX via saturating
/// arithmetic. The loop must break (with a warning log) when the
/// requested range would wrap past 0xFFFF.
#[test]
fn test_parse_w2_form_b_overflow_does_not_collapse() {
    // c_first = 0xFFFB, c_last = 0xFFFF — fits exactly within u16 so
    // every CID in 65531..=65535 must be inserted distinctly. A
    // saturating-add bug would collapse them all onto u16::MAX (and
    // an unchecked-add bug would wrap around to 0).
    let mut dict: HashMap<String, Object> = HashMap::new();
    dict.insert(
        "W2".to_string(),
        Object::Array(vec![
            Object::Integer(0xFFFB),
            Object::Integer(0xFFFF),
            Object::Integer(-1000),
            Object::Integer(500),
            Object::Integer(880),
        ]),
    );
    let metrics = FontInfo::parse_cid_vertical_metrics(&dict, "Test").unwrap();
    let expected = VerticalMetrics {
        w1y: -1000.0,
        v_x: 500.0,
        v_y: 880.0,
    };
    for cid in 0xFFFBu16..=0xFFFFu16 {
        assert_eq!(
            metrics.get(&cid),
            Some(&expected),
            "CID 0x{:04X} must carry the range metrics",
            cid
        );
    }
    // Exactly five distinct CIDs were inserted; nothing else.
    assert_eq!(
        metrics.len(),
        5,
        "Form B near u16::MAX should insert 5 distinct entries; got {}",
        metrics.len()
    );
}

/// `/W2` Form A with a CID start near u16::MAX and an inner array
/// long enough to overflow MUST stop emitting on overflow rather than
/// silently collapsing every subsequent CID onto u16::MAX via
/// saturating arithmetic.
#[test]
fn test_parse_w2_form_a_stops_on_overflow() {
    let mut dict: HashMap<String, Object> = HashMap::new();
    // cid_start = 0xFFFE — only two slots remain (0xFFFE, 0xFFFF) so
    // the third triple would wrap. Confirm we emit exactly two
    // distinct CIDs, not three (which would imply two metrics
    // collapsed onto u16::MAX) and not zero (which would imply a
    // panic-on-overflow bug).
    dict.insert(
        "W2".to_string(),
        Object::Array(vec![
            Object::Integer(0xFFFE),
            Object::Array(vec![
                // CID 0xFFFE
                Object::Integer(-1000),
                Object::Integer(500),
                Object::Integer(880),
                // CID 0xFFFF
                Object::Integer(-900),
                Object::Integer(510),
                Object::Integer(870),
                // CID 0x10000 — overflows; must be DROPPED.
                Object::Integer(-800),
                Object::Integer(520),
                Object::Integer(860),
            ]),
        ]),
    );
    let metrics = FontInfo::parse_cid_vertical_metrics(&dict, "Test").unwrap();
    assert_eq!(
        metrics.get(&0xFFFE),
        Some(&VerticalMetrics {
            w1y: -1000.0,
            v_x: 500.0,
            v_y: 880.0
        })
    );
    assert_eq!(
        metrics.get(&0xFFFF),
        Some(&VerticalMetrics {
            w1y: -900.0,
            v_x: 510.0,
            v_y: 870.0
        })
    );
    assert_eq!(
        metrics.len(),
        2,
        "Form A overflow must drop overflowing triples; got {} entries",
        metrics.len()
    );
}

/// `/DW2` overrides only `v_y` and `w1y`; `v_x` always defaults to
/// `500` per spec (§9.7.4.3 — only two numbers are settable via /DW2).
#[test]
fn test_parse_dw2_overrides_defaults() {
    let mut dict: HashMap<String, Object> = HashMap::new();
    dict.insert(
        "DW2".to_string(),
        Object::Array(vec![Object::Integer(850), Object::Integer(-1100)]),
    );
    let dw2 = FontInfo::parse_dw2(&dict);
    assert_eq!(dw2.v_y, 850.0);
    assert_eq!(dw2.w1y, -1100.0);
    assert_eq!(dw2.v_x, 500.0, "v_x is not settable via /DW2");
}

/// Missing `/DW2` ⇒ spec defaults `(w1y=-1000, v_x=500, v_y=880)`.
#[test]
fn test_parse_dw2_missing_uses_spec_default() {
    let dict: HashMap<String, Object> = HashMap::new();
    assert_eq!(FontInfo::parse_dw2(&dict), VerticalMetrics::SPEC_DEFAULT);
}

/// Malformed `/DW2` (single element instead of two) ⇒ spec defaults.
/// Better to use safe defaults than expose half-parsed metrics that
/// would shift glyph positions in unpredictable ways.
#[test]
fn test_parse_dw2_short_array_uses_spec_default() {
    let mut dict: HashMap<String, Object> = HashMap::new();
    dict.insert("DW2".to_string(), Object::Array(vec![Object::Integer(800)]));
    assert_eq!(FontInfo::parse_dw2(&dict), VerticalMetrics::SPEC_DEFAULT);
}

/// `/DW2` with real-valued numbers parses cleanly.
#[test]
fn test_parse_dw2_real_values() {
    let mut dict: HashMap<String, Object> = HashMap::new();
    dict.insert(
        "DW2".to_string(),
        Object::Array(vec![Object::Real(875.5), Object::Real(-990.25)]),
    );
    let dw2 = FontInfo::parse_dw2(&dict);
    assert_eq!(dw2.v_y, 875.5);
    assert_eq!(dw2.w1y, -990.25);
    assert_eq!(dw2.v_x, 500.0);
}

/// `wmode_from_predefined_cmap_name` returns 1 for any name with a `-V`
/// suffix and for the bare legacy `V`. This is the cheap fast path that
/// avoids parsing the encoding CMap stream when we already know the name
/// declares vertical writing.
#[test]
fn test_wmode_from_predefined_cmap_name_vertical() {
    assert_eq!(wmode_from_predefined_cmap_name("Identity-V"), 1);
    assert_eq!(wmode_from_predefined_cmap_name("UniJIS-UTF16-V"), 1);
    assert_eq!(wmode_from_predefined_cmap_name("UniGB-UTF16-V"), 1);
    assert_eq!(wmode_from_predefined_cmap_name("UniCNS-UTF16-V"), 1);
    assert_eq!(wmode_from_predefined_cmap_name("UniKS-UTF16-V"), 1);
    assert_eq!(wmode_from_predefined_cmap_name("GBK-EUC-V"), 1);
    assert_eq!(wmode_from_predefined_cmap_name("90ms-RKSJ-V"), 1);
    assert_eq!(wmode_from_predefined_cmap_name("V"), 1);
}

/// Horizontal-mode names (the overwhelming majority) must return 0 so
/// the wmode flag stays cold for normal documents.
#[test]
fn test_wmode_from_predefined_cmap_name_horizontal() {
    assert_eq!(wmode_from_predefined_cmap_name("Identity-H"), 0);
    assert_eq!(wmode_from_predefined_cmap_name("UniJIS-UTF16-H"), 0);
    assert_eq!(wmode_from_predefined_cmap_name("UniGB-UTF16-H"), 0);
    assert_eq!(wmode_from_predefined_cmap_name("H"), 0);
    assert_eq!(wmode_from_predefined_cmap_name("WinAnsiEncoding"), 0);
    assert_eq!(wmode_from_predefined_cmap_name("MacRomanEncoding"), 0);
    assert_eq!(wmode_from_predefined_cmap_name("Adobe-Japan1-6"), 0);
    // Edge case: the substring `-V` appears inside but not as a suffix.
    assert_eq!(wmode_from_predefined_cmap_name("V-foo"), 0);
    assert_eq!(wmode_from_predefined_cmap_name("Volt"), 0);
}

/// `FontInfo::get_vertical_metrics` returns per-CID metrics when
/// available, falls back to `/DW2` defaults otherwise. This is the
/// accessor the rasterizer and extractor call on the hot path of every
/// vertical-mode glyph.
#[test]
fn test_get_vertical_metrics_lookup_precedence() {
    let mut per_cid: HashMap<u16, VerticalMetrics> = HashMap::new();
    per_cid.insert(
        7,
        VerticalMetrics {
            w1y: -900.0,
            v_x: 480.0,
            v_y: 870.0,
        },
    );

    let font = make_font(|f| {
        f.subtype = "Type0".to_string();
        f.cid_vertical_metrics = Some(per_cid);
        f.cid_default_vertical_metrics = VerticalMetrics {
            w1y: -1050.0,
            v_x: 500.0,
            v_y: 900.0,
        };
    });

    // Per-CID hit
    assert_eq!(
        font.get_vertical_metrics(7),
        VerticalMetrics {
            w1y: -900.0,
            v_x: 480.0,
            v_y: 870.0
        }
    );
    // Per-CID miss → /DW2 defaults
    assert_eq!(
        font.get_vertical_metrics(99),
        VerticalMetrics {
            w1y: -1050.0,
            v_x: 500.0,
            v_y: 900.0
        }
    );
}

/// When neither `/W2` nor `/DW2` is parsed, `get_vertical_metrics`
/// returns the spec defaults — keeping rendering correct for the common
/// case of a CIDFont that ships only horizontal metrics but is used in
/// a vertical context (caller has already established wmode=1 by name).
#[test]
fn test_get_vertical_metrics_spec_default_fallback() {
    let font = make_font(|f| {
        f.subtype = "Type0".to_string();
        f.cid_vertical_metrics = None;
        f.cid_default_vertical_metrics = VerticalMetrics::SPEC_DEFAULT;
    });
    assert_eq!(
        font.get_vertical_metrics(0x4E00),
        VerticalMetrics::SPEC_DEFAULT
    );
    assert_eq!(VerticalMetrics::SPEC_DEFAULT.w1y, -1000.0);
    assert_eq!(VerticalMetrics::SPEC_DEFAULT.v_x, 500.0);
    assert_eq!(VerticalMetrics::SPEC_DEFAULT.v_y, 880.0);
}
