use super::*;

#[test]
fn classify_normal_is_separable_white_preserving() {
    assert_eq!(
        BlendModeClass::from_name("Normal"),
        BlendModeClass::SeparableWhitePreserving
    );
}

#[test]
fn classify_luminosity_is_non_separable() {
    assert_eq!(
        BlendModeClass::from_name("Luminosity"),
        BlendModeClass::NonSeparable
    );
}

#[test]
fn classify_difference_is_separable_non_white_preserving() {
    assert_eq!(
        BlendModeClass::from_name("Difference"),
        BlendModeClass::SeparableNonWhitePreserving
    );
}

#[test]
fn classify_unknown_falls_back_to_normal_class() {
    // ISO 32000-1 §11.6.3: unknown blend mode names render as
    // /Normal. The classifier reflects that by returning the same
    // class /Normal itself belongs to.
    assert_eq!(
        BlendModeClass::from_name("MarketingInventedMode"),
        BlendModeClass::SeparableWhitePreserving
    );
}

#[test]
fn spot_dispatch_substitutes_normal_for_non_sep_and_non_wp() {
    // §11.7.4.2: only separable AND white-preserving modes apply
    // to spot lanes; every other class substitutes /Normal.
    assert_eq!(
        BlendModeClass::SeparableWhitePreserving.spot_dispatch(),
        SpotBlendDispatch::UseRequested
    );
    assert_eq!(
        BlendModeClass::SeparableNonWhitePreserving.spot_dispatch(),
        SpotBlendDispatch::SubstituteNormal
    );
    assert_eq!(
        BlendModeClass::NonSeparable.spot_dispatch(),
        SpotBlendDispatch::SubstituteNormal
    );
}

#[test]
fn process_dispatch_is_identity_for_every_class() {
    // §11.7.4.2: process lanes always honour the requested BM.
    for class in &[
        BlendModeClass::SeparableWhitePreserving,
        BlendModeClass::SeparableNonWhitePreserving,
        BlendModeClass::NonSeparable,
    ] {
        assert_eq!(class.process_dispatch(), ProcessBlendDispatch::UseRequested);
    }
}

#[test]
fn sidecar_allocates_cmyk_and_spot_planes() {
    let s = CmykSidecar::new(10, 5, vec!["PMS 185 C".into(), "Dieline".into()]);
    assert_eq!(s.dims(), (10, 5));
    assert_eq!(s.cmyk().len(), 4 * 10 * 5);
    assert!(s.cmyk().iter().all(|&b| b == 0));
    assert_eq!(
        s.spot_names(),
        &["PMS 185 C".to_string(), "Dieline".to_string()]
    );
    let p0 = s.spot_plane(0).unwrap();
    let p1 = s.spot_plane(1).unwrap();
    assert_eq!(p0.len(), 10 * 5);
    assert_eq!(p1.len(), 10 * 5);
    assert!(p0.iter().all(|&b| b == 0) && p1.iter().all(|&b| b == 0));
    assert!(s.spot_plane(2).is_none());
}

#[test]
fn sidecar_no_spots_has_zero_length_spot_stack() {
    let s = CmykSidecar::new(7, 3, vec![]);
    assert_eq!(s.dims(), (7, 3));
    assert_eq!(s.cmyk().len(), 4 * 7 * 3);
    assert!(s.spot_names().is_empty());
    assert!(s.spot_plane(0).is_none());
}

/// `process_plate` decomposes the four `DeviceCMYK` channels from
/// the interleaved `(C, M, Y, K)` plane. ISO 32000-1 §10.5: the
/// plate's pixel value equals the subtractive tint of that ink at
/// the pixel. Probe pins per-channel extraction with a synthetic
/// interleaved fill.
#[test]
fn sidecar_process_plate_extracts_named_channel() {
    let mut s = CmykSidecar::new(2, 2, vec![]);
    // Pixel 0: C=10, M=20, Y=30, K=40
    // Pixel 1: C=50, M=60, Y=70, K=80
    // Pixel 2: C=90, M=100, Y=110, K=120
    // Pixel 3: C=130, M=140, Y=150, K=160
    let plane = s.cmyk_mut();
    for (i, v) in plane.iter_mut().enumerate() {
        *v = (i + 10) as u8;
    }
    assert_eq!(
        s.process_plate("Cyan").unwrap(),
        vec![10, 14, 18, 22],
        "Cyan = byte 0 of every interleaved quad starting at 10, +4 per pixel"
    );
    assert_eq!(s.process_plate("Magenta").unwrap(), vec![11, 15, 19, 23]);
    assert_eq!(s.process_plate("Yellow").unwrap(), vec![12, 16, 20, 24]);
    assert_eq!(s.process_plate("Black").unwrap(), vec![13, 17, 21, 25]);
    // Unknown / spot name returns None — spot inks go through
    // spot_plate.
    assert!(s.process_plate("PANTONE 185 C").is_none());
    assert!(s.process_plate("cyan").is_none(), "case-sensitive");
}

/// `spot_plate` borrows the requested spot lane by name. Returns
/// `None` when the ink was not in the discovered spot set.
#[test]
fn sidecar_spot_plate_returns_named_lane() {
    let mut s = CmykSidecar::new(3, 1, vec!["InkA".into(), "InkB".into()]);
    let plane_a = s.spot_plane_mut(0).unwrap();
    plane_a.copy_from_slice(&[10, 20, 30]);
    let plane_b = s.spot_plane_mut(1).unwrap();
    plane_b.copy_from_slice(&[40, 50, 60]);
    assert_eq!(s.spot_plate("InkA").unwrap(), &[10, 20, 30]);
    assert_eq!(s.spot_plate("InkB").unwrap(), &[40, 50, 60]);
    // Not-discovered → None (the §8.6.6.3 "no plate" semantic at
    // the caller).
    assert!(s.spot_plate("InkC").is_none());
}

/// `restore_cmyk` and `restore_spots` overwrite the sidecar's
/// process and spot buffers. Used by the knockout-group cumulative
/// replay to reset lane state to the group's backdrop between
/// element compositions (ISO 32000-1 §11.4.6.2).
#[test]
fn sidecar_restore_cmyk_and_spots_overwrites_buffers() {
    let mut s = CmykSidecar::new(2, 1, vec!["InkA".into()]);
    // Dirty both lanes.
    s.cmyk_mut().copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
    s.spot_plane_mut(0).unwrap().copy_from_slice(&[9, 10]);
    // Snapshots.
    let backdrop_cmyk = vec![100u8; 8];
    let backdrop_spots = vec![50u8; 2];
    s.restore_cmyk(&backdrop_cmyk);
    s.restore_spots(&backdrop_spots);
    assert_eq!(s.cmyk(), backdrop_cmyk.as_slice());
    assert_eq!(s.spots_all(), backdrop_spots.as_slice());
}

/// A test-only `log::Log` that captures every record into a
/// shared buffer. Lets the discover-error probe assert "warn!
/// emitted the expected diagnostic" without pulling in a test
/// crate. `log::set_boxed_logger` is idempotent once-only, so the
/// installation is gated on `OnceLock`.
struct CapturingLogger {
    buf: std::sync::Mutex<Vec<String>>,
}
impl log::Log for CapturingLogger {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= log::Level::Warn
    }
    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            let mut g = self.buf.lock().unwrap();
            g.push(format!("{}", record.args()));
        }
    }
    fn flush(&self) {}
}
static CAPTURING_LOGGER: std::sync::OnceLock<&'static CapturingLogger> = std::sync::OnceLock::new();
fn install_capturing_logger() -> &'static CapturingLogger {
    CAPTURING_LOGGER.get_or_init(|| {
        let leaked: &'static CapturingLogger = Box::leak(Box::new(CapturingLogger {
            buf: std::sync::Mutex::new(Vec::new()),
        }));
        // Tolerate prior installation (other tests may install their own
        // logger first). If installation fails, the buffer stays empty
        // and the probe will fail loudly with a clear message.
        let _ = log::set_logger(leaked);
        log::set_max_level(log::LevelFilter::Warn);
        leaked
    })
}

/// Round-1 QA — surface, don't swallow, the deep-walk error.
///
/// `discover_page_spot_inks` previously called
/// `get_page_inks_deep(...).unwrap_or_default()`, silently mapping
/// every error to an empty vec. A page that genuinely has spots
/// but whose deep walk trips (parse error, recursion bound, page
/// lookup miss) would then allocate a zero-length spot stack — and
/// any downstream paint-op writes to those lanes would quietly
/// drop on the floor.
///
/// The fix emits `log::warn!` on the error path AND returns the
/// empty vec (matching how the separation renderer handles the
/// same `get_page_inks_deep` failure). This probe pins both halves
/// of the contract: empty-vec return, AND a warn record surfaces.
#[test]
fn discover_page_spot_inks_warns_on_deep_walk_error() {
    let logger = install_capturing_logger();
    // Snapshot any prior records so we only inspect ours.
    let start_len = logger.buf.lock().unwrap().len();

    // Single-page synthetic PDF. We will then ask for page 42 — out
    // of range — so `get_page_inks_deep` returns Err on the page
    // tree walk.
    let pdf = b"%PDF-1.4\n\
                    1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n\
                    2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n\
                    3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] >>\nendobj\n\
                    xref\n0 4\n\
                    0000000000 65535 f \n\
                    0000000010 00000 n \n\
                    0000000059 00000 n \n\
                    0000000110 00000 n \n\
                    trailer\n<< /Size 4 /Root 1 0 R >>\nstartxref\n175\n%%EOF\n"
        .to_vec();
    let doc = PdfDocument::from_bytes(pdf).expect("synthetic PDF parses");

    let spots = discover_page_spot_inks(&doc, 42);
    assert!(
        spots.is_empty(),
        "discover_page_spot_inks must return an empty vec on \
             deep-walk error (not panic, not propagate); got {:?}",
        spots
    );

    // The warning message names the page index and includes the
    // word "spot inks" so a log scrape can find it.
    let new_records: Vec<String> = {
        let guard = logger.buf.lock().unwrap();
        guard[start_len..].to_vec()
    };
    let saw_warning = new_records
        .iter()
        .any(|m| m.contains("page 42") && m.contains("spot inks"));
    assert!(
        saw_warning,
        "expected log::warn! naming page 42 and 'spot inks' on the \
             deep-walk error path; captured records since start: {:?}",
        new_records
    );
}

/// Round 5 / B2: `extract_paint_spot_inks` for a Pattern colour
/// space with a /Separation underlying. The Pattern array form is
/// `[/Pattern <underlying-cs>]`; the underlying may be any colour
/// space (uncoloured Tiling). When the underlying is a /Separation
/// or /DeviceN with a spot colorant, the spot identity MUST
/// propagate to the dispatcher so the spot mirror writes the
/// correct lane.
///
/// Spec citations:
///  - §8.7.3.1 — Pattern colour space (uncoloured Tiling carries
///    the underlying colour space's tints)
///  - §8.6.6.3 — /Separation spot identity
///  - §11.7.3   — single shape/opacity per pixel across lanes
#[test]
fn extract_paint_spot_inks_pattern_with_separation_underlying() {
    // Build the colour-space object: [/Pattern [/Separation
    // /PMS185 /DeviceCMYK <stub tint fn>]]. The stub tint fn is a
    // bare dict — the extractor does not consult it; the
    // dispatcher only reads /Separation's index-1 name and uses
    // the components vector for the tint.
    let tint_fn = Object::Dictionary(
        [
            ("FunctionType".to_string(), Object::Integer(2)),
            (
                "Domain".to_string(),
                Object::Array(vec![Object::Integer(0), Object::Integer(1)]),
            ),
            ("C0".to_string(), Object::Array(vec![Object::Integer(0); 4])),
            ("C1".to_string(), Object::Array(vec![Object::Integer(1); 4])),
            ("N".to_string(), Object::Integer(1)),
        ]
        .into_iter()
        .collect(),
    );
    let underlying = Object::Array(vec![
        Object::Name("Separation".to_string()),
        Object::Name("PMS185".to_string()),
        Object::Name("DeviceCMYK".to_string()),
        tint_fn,
    ]);
    let pattern_cs = Object::Array(vec![Object::Name("Pattern".to_string()), underlying]);

    // Minimal PDF for the doc context. The extractor only calls
    // resolve_object on indirect refs; the inline objects above
    // need no resolution.
    let pdf: Vec<u8> = b"%PDF-1.4\n\
                             1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n\
                             2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n\
                             3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] >>\nendobj\n\
                             xref\n0 4\n\
                             0000000000 65535 f \n\
                             0000000010 00000 n \n\
                             0000000059 00000 n \n\
                             0000000110 00000 n \n\
                             trailer\n<< /Size 4 /Root 1 0 R >>\nstartxref\n175\n%%EOF\n"
            .to_vec();
    let doc = PdfDocument::from_bytes(pdf).expect("synthetic PDF parses");

    // Components: the underlying /Separation expects one tint.
    let components = [0.6_f32];
    let spots = extract_paint_spot_inks(&pattern_cs, &components, &doc);

    assert_eq!(
        spots.len(),
        1,
        "ISO 32000-1 §8.7.3.1: Pattern[/Separation /PMS185 …] must \
             surface PMS185 via the underlying-space recursion. Got \
             {} entries; expected 1.",
        spots.len()
    );
    assert_eq!(spots[0].0, "PMS185", "spot identity propagation");
    assert_eq!(
        spots[0].1, 0.6_f32,
        "spot tint propagation (0.6_f32 is exact in f32)"
    );
}

/// Round 5 / A5: the `process_names_if_valid_prefix` helper
/// returns the /Components set ONLY when every name appears in
/// /Names; otherwise it returns empty (treating the /Process
/// attribution as inert per
/// HONEST_GAP_DEVICEN_PROCESS_MISMATCHED_NAMES). Probe pins both
/// arms.
#[test]
fn process_names_if_valid_prefix_returns_set_for_valid_prefix() {
    let deref = |o: &Object| -> Object { o.clone() };
    let names = vec![
        Object::Name("Cyan".to_string()),
        Object::Name("Magenta".to_string()),
        Object::Name("Yellow".to_string()),
        Object::Name("Black".to_string()),
        Object::Name("PMS185".to_string()),
    ];
    let attrs = Object::Dictionary(
        [(
            "Process".to_string(),
            Object::Dictionary(
                [
                    (
                        "ColorSpace".to_string(),
                        Object::Name("DeviceCMYK".to_string()),
                    ),
                    (
                        "Components".to_string(),
                        Object::Array(vec![
                            Object::Name("Cyan".to_string()),
                            Object::Name("Magenta".to_string()),
                            Object::Name("Yellow".to_string()),
                            Object::Name("Black".to_string()),
                        ]),
                    ),
                ]
                .into_iter()
                .collect(),
            ),
        )]
        .into_iter()
        .collect(),
    );
    let cs_arr = vec![
        Object::Name("DeviceN".to_string()),
        Object::Array(names.clone()),
        Object::Name("DeviceCMYK".to_string()),
        // tint transform placeholder
        Object::Null,
        attrs,
    ];
    let result = process_names_if_valid_prefix(&cs_arr, &names, &deref);
    let expected: std::collections::HashSet<String> = ["Cyan", "Magenta", "Yellow", "Black"]
        .into_iter()
        .map(str::to_string)
        .collect();
    assert_eq!(result, expected, "valid prefix returns the /Components set");
}

#[test]
fn process_names_if_valid_prefix_returns_empty_for_invalid_prefix() {
    let deref = |o: &Object| -> Object { o.clone() };
    let names = vec![
        Object::Name("Cyan".to_string()),
        Object::Name("Magenta".to_string()),
        Object::Name("Yellow".to_string()),
        Object::Name("Black".to_string()),
    ];
    let attrs = Object::Dictionary(
        [(
            "Process".to_string(),
            Object::Dictionary(
                [
                    (
                        "ColorSpace".to_string(),
                        Object::Name("DeviceCMYK".to_string()),
                    ),
                    (
                        "Components".to_string(),
                        Object::Array(vec![
                            Object::Name("Cyan".to_string()),
                            Object::Name("Magenta".to_string()),
                            Object::Name("Yellow".to_string()),
                            // /Iridescent NOT in /Names → malformed
                            Object::Name("Iridescent".to_string()),
                        ]),
                    ),
                ]
                .into_iter()
                .collect(),
            ),
        )]
        .into_iter()
        .collect(),
    );
    let cs_arr = vec![
        Object::Name("DeviceN".to_string()),
        Object::Array(names.clone()),
        Object::Name("DeviceCMYK".to_string()),
        Object::Null,
        attrs,
    ];
    let result = process_names_if_valid_prefix(&cs_arr, &names, &deref);
    assert!(
        result.is_empty(),
        "ISO 32000-1 §8.6.6.5 violation (one name not in /Names) \
             must return empty per HONEST_GAP_DEVICEN_PROCESS_MISMATCHED\
             _NAMES. Got {:?}.",
        result
    );
}

// ============================================================
// Detection-helper indirect-ref + nested-form regressions (M3).
// ============================================================
//
// `page_declares_transparency_or_overprint` /
// `page_declares_transparency` previously read `/CA /ca /SMask /BM`
// straight off the ExtGState dict and only inspected the page-
// level resource scope. Two PDF shapes silently routed through
// the per-plate walker:
//
//   1. ExtGState whose `/CA /ca /BM` value is an indirect
//      reference (the resolved name / number triggers transparency
//      but the raw Reference variant fell through the `match` to
//      `_ => 1.0` / unrecognised mode).
//   2. Form XObject whose own `/Resources/ExtGState` declares a
//      transparent entry, with the page-level ExtGState empty.
//
// The probes below construct minimal synthetic PDFs that
// surface each case and assert the detection helper now returns
// `true`. Sensitivity verification: stash the corresponding fix
// → assertion flips to false.

/// Build a single-page PDF whose page-level Resources dict carries
/// the literal text in `resources_inner` (e.g.
/// `"/ExtGState << /T << /Type /ExtGState /ca 6 0 R >> >>"`) and
/// whose object table includes the verbatim `extra_objs` after the
/// page-content stream. Returns the parsed `PdfDocument` and the
/// page's `/Resources` dictionary so callers can hand both to
/// `page_declares_transparency_*`.
fn build_doc_with_resources_and_objs(
    resources_inner: &str,
    extra_objs: &[&str],
) -> (PdfDocument, Object) {
    let mut buf: Vec<u8> = Vec::new();
    buf.extend_from_slice(b"%PDF-1.4\n");
    let cat_off = buf.len();
    buf.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");
    let pages_off = buf.len();
    buf.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n");
    let page_off = buf.len();
    let page = format!(
        "3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] \
             /Resources << {} >> /Contents 4 0 R >>\nendobj\n",
        resources_inner
    );
    buf.extend_from_slice(page.as_bytes());
    let stream_off = buf.len();
    let body = b"% no content\n";
    let stream_hdr = format!("4 0 obj\n<< /Length {} >>\nstream\n", body.len());
    buf.extend_from_slice(stream_hdr.as_bytes());
    buf.extend_from_slice(body);
    buf.extend_from_slice(b"\nendstream\nendobj\n");

    let mut extra_offs: Vec<usize> = Vec::new();
    for obj in extra_objs {
        extra_offs.push(buf.len());
        buf.extend_from_slice(obj.as_bytes());
    }

    let xref_off = buf.len();
    let total_objs = 4 + extra_objs.len();
    buf.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", total_objs + 1).as_bytes());
    for off in [cat_off, pages_off, page_off, stream_off] {
        buf.extend_from_slice(format!("{:010} 00000 n \n", off).as_bytes());
    }
    for off in extra_offs {
        buf.extend_from_slice(format!("{:010} 00000 n \n", off).as_bytes());
    }
    buf.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
            total_objs + 1,
            xref_off
        )
        .as_bytes(),
    );

    let doc = PdfDocument::from_bytes(buf).expect("synthetic PDF parses");
    let resources = doc.get_page_resources(0).expect("page resources");
    (doc, resources)
}

#[test]
fn detection_resolves_indirect_ca() {
    // `/ca 6 0 R` where 6 0 obj is `Real(0.6)`. Pre-fix: the
    // `match v` arm on `Object::Reference` fell to `_ => 1.0`,
    // alpha stayed 1.0, the helper missed the trigger.
    let resources_inner = "/ExtGState << /T << /Type /ExtGState /ca 6 0 R >> >>";
    let extras = ["6 0 obj\n0.6\nendobj\n"];
    let (doc, resources) = build_doc_with_resources_and_objs(resources_inner, &extras);
    assert!(
        page_declares_transparency_or_overprint(&doc, &resources),
        "page_declares_transparency_or_overprint must dereference \
             `/ca 6 0 R` and recognise the resolved Real(0.6) < 1.0 \
             as transparent."
    );
    assert!(
        page_declares_transparency(&doc, &resources),
        "page_declares_transparency must dereference `/ca 6 0 R` \
             and recognise the resolved Real(0.6) < 1.0 as transparent."
    );
}

#[test]
fn detection_resolves_indirect_ca_upper() {
    // /CA mirror of /ca.
    let resources_inner = "/ExtGState << /T << /Type /ExtGState /CA 6 0 R >> >>";
    let extras = ["6 0 obj\n0.7\nendobj\n"];
    let (doc, resources) = build_doc_with_resources_and_objs(resources_inner, &extras);
    assert!(
        page_declares_transparency_or_overprint(&doc, &resources),
        "page_declares_transparency_or_overprint must dereference \
             `/CA 6 0 R` and recognise the resolved Real(0.7) < 1.0 \
             as transparent."
    );
}

#[test]
fn detection_resolves_indirect_bm() {
    // `/BM 6 0 R` where 6 0 obj is `Name("Multiply")`. Pre-fix:
    // `bm_is_non_normal` matched against `Object::Reference` and
    // returned `false`, missing the trigger.
    let resources_inner = "/ExtGState << /T << /Type /ExtGState /BM 6 0 R >> >>";
    let extras = ["6 0 obj\n/Multiply\nendobj\n"];
    let (doc, resources) = build_doc_with_resources_and_objs(resources_inner, &extras);
    assert!(
        page_declares_transparency_or_overprint(&doc, &resources),
        "page_declares_transparency_or_overprint must dereference \
             `/BM 6 0 R` and recognise the resolved /Multiply name as \
             non-/Normal."
    );
}

#[test]
fn detection_recurses_into_form_xobject_extgstate() {
    // Form XObject (object 6) whose own /Resources/ExtGState
    // declares a transparent state (/ca 0.6). Page-level
    // ExtGState is empty. Pre-fix: the XObject loop checked only
    // /Group and /SMask on the form dict, missing the nested
    // transparency entirely.
    let form_obj = "6 0 obj\n\
            << /Type /XObject /Subtype /Form /FormType 1 \
               /BBox [0 0 100 100] \
               /Resources << /ExtGState << /Half << /Type /ExtGState /ca 0.6 >> >> >> \
               /Length 14 >>\n\
            stream\n% no paint\n\nendstream\nendobj\n";
    let resources_inner = "/XObject << /F 6 0 R >>";
    let (doc, resources) = build_doc_with_resources_and_objs(resources_inner, &[form_obj]);
    assert!(
        page_declares_transparency_or_overprint(&doc, &resources),
        "page_declares_transparency_or_overprint must recurse into \
             Form-XObject /Resources/ExtGState. The form's /Half \
             ExtGState declares /ca 0.6; the page must route through \
             composite-then-decompose."
    );
    assert!(
        page_declares_transparency(&doc, &resources),
        "narrower page_declares_transparency must also recurse \
             into nested-form ExtGState."
    );
}

#[test]
fn detection_no_trigger_returns_false() {
    // Sanity: a page with neither ExtGState nor XObject still
    // reports false (no regressions from the recursion shape).
    let resources_inner = "/ColorSpace << /CS [/Separation /InkA /DeviceCMYK << >>] >>";
    let (doc, resources) = build_doc_with_resources_and_objs(resources_inner, &[]);
    assert!(
        !page_declares_transparency_or_overprint(&doc, &resources),
        "no ExtGState or XObject → no transparency / overprint trigger."
    );
    assert!(
        !page_declares_transparency(&doc, &resources),
        "no ExtGState or XObject → no transparency-only trigger."
    );
}
