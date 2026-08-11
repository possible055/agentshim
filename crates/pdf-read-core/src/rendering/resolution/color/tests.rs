use super::*;
use crate::rendering::resolution::test_support::fixture_doc;
use std::collections::HashMap;

fn ctx<'a>(
    doc: &'a crate::document::PdfDocument,
    spaces: &'a HashMap<String, Object>,
) -> ResolutionContext<'a> {
    ResolutionContext::new(doc, spaces)
}

/// Assert resolved colour matches expected RGBA. Accepts either
/// `ResolvedColor::Rgba` directly or `ResolvedColor::Cmyk`
/// projected via the same process-ink `cmyk_to_rgb` the composite
/// render path uses (the resolver now emits Cmyk for Separation /
/// DeviceN sources with a CMYK alternate so per-plate backends see
/// the channel decomposition; composite consumers project on
/// demand). Projecting through the engine's own converter keeps the
/// expected RGB in this helper consistent with what the renderer
/// actually paints for the same CMYK plates.
fn assert_rgba(c: ResolvedColor, r: f32, g: f32, b: f32, a: f32) {
    let (rr, gg, bb, aa) = match c {
        ResolvedColor::Rgba { r, g, b, a } => (r, g, b, a),
        ResolvedColor::Cmyk { c, m, y, k, a } => {
            let (rr, gg, bb) = super::cmyk_to_rgb(c, m, y, k);
            (rr, gg, bb, a)
        }
        other => panic!("expected Rgba or Cmyk; got {other:?}"),
    };
    assert!((rr - r).abs() < 1e-3, "r: got {rr}, want {r}");
    assert!((gg - g).abs() < 1e-3, "g: got {gg}, want {g}");
    assert!((bb - b).abs() < 1e-3, "b: got {bb}, want {b}");
    assert!((aa - a).abs() < 1e-3, "a: got {aa}, want {a}");
}

#[test]
fn resolves_device_gray_logical_color() {
    let doc = fixture_doc();
    let spaces = HashMap::new();
    let resolver = ColorResolver::new();
    let lc = LogicalColor::Device(DeviceColor::Gray(0.42));
    let c = resolver.resolve(&lc, &ctx(&doc, &spaces), 0.9).unwrap();
    assert_rgba(c, 0.42, 0.42, 0.42, 0.9);
}

#[test]
fn resolves_device_rgb_logical_color() {
    let doc = fixture_doc();
    let spaces = HashMap::new();
    let resolver = ColorResolver::new();
    let lc = LogicalColor::Device(DeviceColor::Rgb(1.0, 0.5, 0.25));
    let c = resolver.resolve(&lc, &ctx(&doc, &spaces), 1.0).unwrap();
    assert_rgba(c, 1.0, 0.5, 0.25, 1.0);
}

#[test]
fn resolves_device_cmyk_via_process_inks() {
    // DeviceCMYK composites through the process-ink converter
    // (`crate::color::cmyk_to_rgb`, tetralinear over the 16 measured
    // ink corners), NOT the §10.3.5 additive clamp. 100% cyan lands
    // on the measured corner `#00ADEF` = (0.0, 0.6784, 0.9373), not
    // (0, 1, 1). The resolver emits `Cmyk` (for per-plate routing);
    // the composite projection is `cmyk_to_rgb_via_intent`, whose
    // no-OutputIntent fallback is the process-ink path.
    let doc = fixture_doc();
    let spaces = HashMap::new();
    let resolver = ColorResolver::new();
    let lc = LogicalColor::Device(DeviceColor::Cmyk(1.0, 0.0, 0.0, 0.0));
    let c = resolver.resolve(&lc, &ctx(&doc, &spaces), 1.0).unwrap();
    let (cc, m, y, k, a) = match c {
        ResolvedColor::Cmyk { c, m, y, k, a } => (c, m, y, k, a),
        other => panic!("expected Cmyk; got {other:?}"),
    };
    let (r, g, b) = super::cmyk_to_rgb_via_intent(cc, m, y, k, &ctx(&doc, &spaces));
    assert!((r - 0.0).abs() < 1e-3, "r: got {r}, want 0.0");
    assert!((g - 0.6784).abs() < 1e-3, "g: got {g}, want 0.6784");
    assert!((b - 0.9373).abs() < 1e-3, "b: got {b}, want 0.9373");
    assert!((a - 1.0).abs() < 1e-3, "a: got {a}, want 1.0");
}

#[test]
fn resolves_spaced_device_alias_as_rgb() {
    let doc = fixture_doc();
    let spaces = HashMap::new();
    let resolver = ColorResolver::new();
    let space = Object::Name("DeviceRGB".to_string());
    let lc = LogicalColor::Spaced {
        space: &space,
        components: smallvec::smallvec![0.2, 0.4, 0.6],
    };
    let c = resolver.resolve(&lc, &ctx(&doc, &spaces), 1.0).unwrap();
    assert_rgba(c, 0.2, 0.4, 0.6, 1.0);
}

#[test]
fn separation_with_type2_cmyk_alternate_uses_function() {
    // /Separation /SpotInk /DeviceCMYK
    //   << /FunctionType 2 /N 1 /C0 [0 0 0 0] /C1 [0 1 0 0] /Domain [0 1] /Range [0 1 0 1 0 1 0 1] >>
    // tint=1 must produce CMYK(0,1,0,0), the process-ink magenta
    // corner #EC008C = (0.9255, 0, 0.5490).
    let mut func_dict: HashMap<String, Object> = HashMap::new();
    func_dict.insert("FunctionType".into(), Object::Integer(2));
    func_dict.insert("N".into(), Object::Integer(1));
    func_dict.insert(
        "C0".into(),
        Object::Array(vec![
            Object::Real(0.0),
            Object::Real(0.0),
            Object::Real(0.0),
            Object::Real(0.0),
        ]),
    );
    func_dict.insert(
        "C1".into(),
        Object::Array(vec![
            Object::Real(0.0),
            Object::Real(1.0),
            Object::Real(0.0),
            Object::Real(0.0),
        ]),
    );
    let func_obj = Object::Dictionary(func_dict);

    let arr = vec![
        Object::Name("Separation".into()),
        Object::Name("SpotInk".into()),
        Object::Name("DeviceCMYK".into()),
        func_obj,
    ];
    let space = Object::Array(arr);
    let doc = fixture_doc();
    let spaces = HashMap::new();
    let resolver = ColorResolver::new();
    let lc = LogicalColor::Spaced {
        space: &space,
        components: smallvec::smallvec![1.0],
    };
    let c = resolver.resolve(&lc, &ctx(&doc, &spaces), 1.0).unwrap();
    // CMYK(0,1,0,0) -> process-ink magenta corner (0.9255, 0, 0.5490)
    assert_rgba(c, 0.9255, 0.0, 0.5490, 1.0);
}

#[test]
fn separation_with_type4_calculator_evaluates_program() {
    // /Separation /MagentaSpot /DeviceCMYK
    //   stream containing: { 0.0 exch dup 0.0 exch 0.0 }  ; tint → CMYK(0, tint, 0, 0)
    // tint=1.0 should yield CMYK(0,1,0,0) → RGB(1,0,1).
    //
    // This is the canonical test for the PR #630 case: the existing inline
    // path at page_renderer.rs:690 returns `1.0 - tint` = 0.0 (solid black)
    // because it only recognises FunctionType==2. Through the resolver,
    // the Type-4 program runs to completion and the colour comes out
    // correct.
    //
    // PostScript stack convention: inputs are pushed in order, output is
    // read top-down from the final stack. With one input (tint) the
    // program needs to leave four values on the stack representing
    // C, M, Y, K. We use: `0.0 exch 0.0 0.0` — tint is on top after
    // exch, but we want the order C M Y K = 0 tint 0 0. The simplest
    // form: pop the tint into M position by emitting `0.0 3 1 roll
    // 0.0 0.0` doesn't actually work cleanly; instead use:
    //   `{ 0.0 exch 0.0 0.0 }` — wait this pushes 0, then swaps with
    //   tint giving stack [tint, 0], then pushes 0 0 giving
    //   [tint, 0, 0, 0]. That's C=tint not M=tint.
    //
    // To get [C, M, Y, K] = [0, tint, 0, 0] in PLRM stack order
    // (output order top-down so K is top), we need stack contents
    // bottom-to-top: [0, tint, 0, 0]. With tint on the stack from the
    // caller, we want: push 0 below tint (using exch), then push 0 0.
    // That's `0 exch 0 0` — yields stack bottom-to-top [0, tint, 0, 0],
    // i.e. C=0, M=tint, Y=0, K=0. (`evaluate_type4` returns the stack
    // from bottom to top as a Vec, so out[0]=C, out[1]=M, out[2]=Y,
    // out[3]=K.)
    let program = b"{ 0.0 exch 0.0 0.0 }";

    let mut func_dict: HashMap<String, Object> = HashMap::new();
    func_dict.insert("FunctionType".into(), Object::Integer(4));
    func_dict.insert(
        "Domain".into(),
        Object::Array(vec![Object::Integer(0), Object::Integer(1)]),
    );
    func_dict.insert(
        "Range".into(),
        Object::Array(vec![
            Object::Integer(0),
            Object::Integer(1),
            Object::Integer(0),
            Object::Integer(1),
            Object::Integer(0),
            Object::Integer(1),
            Object::Integer(0),
            Object::Integer(1),
        ]),
    );

    let func_obj = Object::Stream {
        dict: func_dict,
        data: program.to_vec().into(),
    };

    let arr = vec![
        Object::Name("Separation".into()),
        Object::Name("MagentaSpot".into()),
        Object::Name("DeviceCMYK".into()),
        func_obj,
    ];
    let space = Object::Array(arr);
    let doc = fixture_doc();
    let spaces = HashMap::new();
    let resolver = ColorResolver::new();
    let lc = LogicalColor::Spaced {
        space: &space,
        components: smallvec::smallvec![1.0],
    };
    let c = resolver.resolve(&lc, &ctx(&doc, &spaces), 1.0).unwrap();
    assert_rgba(c, 0.9255, 0.0, 0.5490, 1.0);
}

#[test]
fn separation_full_tint_with_type4_no_longer_renders_solid_black() {
    // Regression guard for the structural class of bug demonstrated by
    // PR #630: a Separation with a Type-4 tint transform and a fully
    // opaque tint must not fall back to the `1.0 - tint = 0` grayscale
    // path. The previous test confirmed the resolved RGB is non-black;
    // this test asserts directly that none of the channels are zero
    // luminance, regardless of the specific colour produced.
    //
    // Program: `{ 0.0 exch 0.0 0.0 }` again — yields CMYK(0, tint, 0, 0),
    // RGB(1-0, 1-tint, 1-0) = (1, 1-tint, 1). At tint=1, that's (1, 0, 1).
    let program = b"{ 0.0 exch 0.0 0.0 }";
    let mut func_dict: HashMap<String, Object> = HashMap::new();
    func_dict.insert("FunctionType".into(), Object::Integer(4));
    let func_obj = Object::Stream {
        dict: func_dict,
        data: program.to_vec().into(),
    };
    let arr = vec![
        Object::Name("Separation".into()),
        Object::Name("MagentaSpot".into()),
        Object::Name("DeviceCMYK".into()),
        func_obj,
    ];
    let space = Object::Array(arr);
    let doc = fixture_doc();
    let spaces = HashMap::new();
    let resolver = ColorResolver::new();
    let lc = LogicalColor::Spaced {
        space: &space,
        components: smallvec::smallvec![1.0],
    };
    let c = resolver.resolve(&lc, &ctx(&doc, &spaces), 1.0).unwrap();
    // Separation with a DeviceCMYK alternate now emits Cmyk so the
    // per-plate router can route channels by name. Project the
    // result to RGBA for the regression-guard comparison.
    let (r, g, b) = match c {
        ResolvedColor::Rgba { r, g, b, .. } => (r, g, b),
        ResolvedColor::Cmyk { c, m, y, k, .. } => {
            let rr = (1.0 - (c + k).min(1.0)).clamp(0.0, 1.0);
            let gg = (1.0 - (m + k).min(1.0)).clamp(0.0, 1.0);
            let bb = (1.0 - (y + k).min(1.0)).clamp(0.0, 1.0);
            (rr, gg, bb)
        }
        other => panic!("expected Rgba or Cmyk; got {other:?}"),
    };
    // The old inline path would have produced gray = 1.0 - 1.0 = 0.0
    // for all channels. The pipeline must never produce that for a
    // Type-4 spot.
    assert!(
        !(r < 0.01 && g < 0.01 && b < 0.01),
        "full-tint Type-4 spot must not render solid black; got ({r}, {g}, {b})"
    );
}

#[test]
fn type0_sampled_function_single_dimension_matches_prior_linear_interpolation() {
    // Pins the pre-existing single-input behaviour exactly: 3 samples
    // over Domain [0 1], input 0.25 sits 50% of the way between sample
    // 0 (0/255) and sample 1 (128/255).
    let mut func_dict: HashMap<String, Object> = HashMap::new();
    func_dict.insert("FunctionType".into(), Object::Integer(0));
    func_dict.insert(
        "Domain".into(),
        Object::Array(vec![Object::Integer(0), Object::Integer(1)]),
    );
    func_dict.insert(
        "Range".into(),
        Object::Array(vec![Object::Integer(0), Object::Integer(1)]),
    );
    func_dict.insert("Size".into(), Object::Array(vec![Object::Integer(3)]));
    func_dict.insert("BitsPerSample".into(), Object::Integer(8));
    let func_obj = Object::Stream {
        dict: func_dict,
        data: vec![0u8, 128, 255].into(),
    };
    let out = super::evaluate_type0_sampled(&func_obj, &[0.25]).expect("in supported envelope");
    assert_eq!(out.len(), 1);
    let expected = 0.5 * (128.0 / 255.0);
    assert!(
        (out[0] - expected).abs() < 1e-4,
        "got {}, want {}",
        out[0],
        expected
    );
}

#[test]
fn type0_sampled_function_two_dimensional_input_uses_all_components() {
    // A genuinely 2-D sampled function (Size [2 2]): the sample only
    // takes a non-zero value at the (1,1) grid corner. Feeding inputs
    // [0.25, 0.75] must land at 0.25*0.75 = 0.1875 — the bilinear
    // weight of that corner — which is impossible to produce from
    // `components[0]` alone (a single-input reading would ignore the
    // second component entirely and could never reach a component-1
    // dependent value).
    let mut func_dict: HashMap<String, Object> = HashMap::new();
    func_dict.insert("FunctionType".into(), Object::Integer(0));
    func_dict.insert(
        "Domain".into(),
        Object::Array(vec![
            Object::Integer(0),
            Object::Integer(1),
            Object::Integer(0),
            Object::Integer(1),
        ]),
    );
    func_dict.insert(
        "Range".into(),
        Object::Array(vec![Object::Integer(0), Object::Integer(1)]),
    );
    func_dict.insert(
        "Size".into(),
        Object::Array(vec![Object::Integer(2), Object::Integer(2)]),
    );
    func_dict.insert("BitsPerSample".into(), Object::Integer(8));
    // Sample order per SS 7.10.2 (dim 0 fastest): (0,0) (1,0) (0,1) (1,1).
    let func_obj = Object::Stream {
        dict: func_dict,
        data: vec![0u8, 0, 0, 255].into(),
    };
    let out =
        super::evaluate_type0_sampled(&func_obj, &[0.25, 0.75]).expect("in supported envelope");
    assert_eq!(out.len(), 1);
    assert!(
        (out[0] - 0.1875).abs() < 1e-4,
        "got {}, want 0.1875",
        out[0]
    );

    // Sanity: the OLD single-input fallback formula (1 - components[0])
    // would have produced 0.75 here — a different value — confirming
    // this assertion actually exercises the second input dimension
    // rather than coincidentally matching the discarded-component path.
    assert!((out[0] - 0.75).abs() > 1e-3);
}

#[test]
fn devicen_two_channel_type0_tint_transform_resolves_via_all_components() {
    // End-to-end: a DeviceN colour space with 2 named channels and a
    // Type 0 (sampled) tint transform, resolved through the full
    // Separation/DeviceN pipeline exactly as a `scn` operator would
    // drive it. Mirrors the structure of real-world multi-channel
    // DeviceN spot-colour PDFs (a sampled tint transform over an N>1
    // colorant set).
    let mut func_dict: HashMap<String, Object> = HashMap::new();
    func_dict.insert("FunctionType".into(), Object::Integer(0));
    func_dict.insert(
        "Domain".into(),
        Object::Array(vec![
            Object::Integer(0),
            Object::Integer(1),
            Object::Integer(0),
            Object::Integer(1),
        ]),
    );
    func_dict.insert(
        "Range".into(),
        Object::Array(vec![
            Object::Integer(0),
            Object::Integer(1),
            Object::Integer(0),
            Object::Integer(1),
            Object::Integer(0),
            Object::Integer(1),
        ]),
    );
    func_dict.insert(
        "Size".into(),
        Object::Array(vec![Object::Integer(2), Object::Integer(2)]),
    );
    func_dict.insert("BitsPerSample".into(), Object::Integer(8));
    // 3 output channels (R, G, B on the DeviceRGB alternate), 4 grid
    // corners. Only the (1,1) corner (both inputs at their max) is
    // pure red; every other corner is black.
    #[rustfmt::skip]
        let samples: Vec<u8> = vec![
            0, 0, 0,       // (0,0)
            0, 0, 0,       // (1,0)
            0, 0, 0,       // (0,1)
            255, 0, 0,     // (1,1)
        ];
    let func_obj = Object::Stream {
        dict: func_dict,
        data: samples.into(),
    };
    let arr = vec![
        Object::Name("DeviceN".into()),
        Object::Array(vec![
            Object::Name("Alpha".into()),
            Object::Name("Beta".into()),
        ]),
        Object::Name("DeviceRGB".into()),
        func_obj,
    ];
    let space = Object::Array(arr);
    let doc = fixture_doc();
    let spaces = HashMap::new();
    let resolver = ColorResolver::new();
    let lc = LogicalColor::Spaced {
        space: &space,
        components: smallvec::smallvec![1.0, 1.0],
    };
    let c = resolver.resolve(&lc, &ctx(&doc, &spaces), 1.0).unwrap();
    assert_rgba(c, 1.0, 0.0, 0.0, 1.0);
}

#[test]
fn separation_none_resolves_to_fully_transparent_for_composite() {
    // §8.6.6.3 reserved name `/None`: composite output is fully
    // transparent so the splice carries no marks through, mirroring
    // the per-plate `Skip` decision the InkRouter makes off the
    // OverprintPlan's `selector: InkSelector::None`.
    let arr = vec![
        Object::Name("Separation".into()),
        Object::Name("None".into()),
        Object::Name("DeviceGray".into()),
        Object::Dictionary({
            let mut d = HashMap::new();
            d.insert("FunctionType".into(), Object::Integer(2));
            d
        }),
    ];
    let space = Object::Array(arr);
    let doc = fixture_doc();
    let spaces = HashMap::new();
    let resolver = ColorResolver::new();
    let lc = LogicalColor::Spaced {
        space: &space,
        components: smallvec::smallvec![0.5],
    };
    let c = resolver.resolve(&lc, &ctx(&doc, &spaces), 0.9).unwrap();
    match c {
        ResolvedColor::Rgba { a, .. } => {
            assert!((a - 0.0).abs() < 1e-6, "/None composite alpha must be 0");
        }
        other => panic!("expected Rgba; got {other:?}"),
    }
}

#[test]
fn separation_with_unknown_function_type_falls_back_to_gray() {
    // FunctionType 99 is not a real PDF spec value; the resolver must
    // degrade safely rather than panic. Matches the existing inline
    // behaviour of "first component as gray".
    let mut func_dict: HashMap<String, Object> = HashMap::new();
    func_dict.insert("FunctionType".into(), Object::Integer(99));
    let func_obj = Object::Dictionary(func_dict);
    let arr = vec![
        Object::Name("Separation".into()),
        Object::Name("Whatever".into()),
        Object::Name("DeviceCMYK".into()),
        func_obj,
    ];
    let space = Object::Array(arr);
    let doc = fixture_doc();
    let spaces = HashMap::new();
    let resolver = ColorResolver::new();
    let lc = LogicalColor::Spaced {
        space: &space,
        components: smallvec::smallvec![0.5],
    };
    let c = resolver.resolve(&lc, &ctx(&doc, &spaces), 1.0).unwrap();
    // First component as gray: g = 0.5
    assert_rgba(c, 0.5, 0.5, 0.5, 1.0);
}

#[test]
fn iccbased_with_n4_routes_through_cmyk_fallback() {
    // ICCBased streams declare /N. With N=4 we treat components as
    // DeviceCMYK in the no-CMM fallback path (same as the existing
    // inline behaviour at `page_renderer.rs:584-617`).
    let mut stream_dict: HashMap<String, Object> = HashMap::new();
    stream_dict.insert("N".into(), Object::Integer(4));
    let icc_stream = Object::Stream {
        dict: stream_dict,
        data: Vec::new().into(),
    };
    let arr = vec![Object::Name("ICCBased".into()), icc_stream];
    let space = Object::Array(arr);
    let doc = fixture_doc();
    let spaces = HashMap::new();
    let resolver = ColorResolver::new();
    let lc = LogicalColor::Spaced {
        space: &space,
        components: smallvec::smallvec![1.0, 0.0, 0.0, 0.0],
    };
    let c = resolver.resolve(&lc, &ctx(&doc, &spaces), 1.0).unwrap();
    // ICCBased N=4 falls back to DeviceCMYK; CMYK(1,0,0,0) composites
    // through the process-ink converter to the cyan corner #00ADEF.
    assert_rgba(c, 0.0, 0.6784, 0.9373, 1.0);
}

#[test]
fn alpha_passthrough_into_rgba() {
    // Every resolution path must fold the input alpha into the output
    // RGBA. Test the Device path here; the rest is covered by the
    // type-specific tests above.
    let doc = fixture_doc();
    let spaces = HashMap::new();
    let resolver = ColorResolver::new();
    let lc = LogicalColor::Device(DeviceColor::Gray(0.5));
    let c = resolver.resolve(&lc, &ctx(&doc, &spaces), 0.3).unwrap();
    match c {
        ResolvedColor::Rgba { a, .. } => assert!((a - 0.3).abs() < 1e-6),
        _ => panic!("expected Rgba"),
    }
}

#[test]
fn cmyk_to_rgb_via_intent_with_no_output_intent_uses_process_inks() {
    // The fallback arm is the process-ink `cmyk_to_rgb`. Pin one
    // representative quadruple so a regression that re-routed the
    // no-OutputIntent path through some other conversion (e.g. back
    // to the §10.3.5 additive clamp) would surface here. CMYK(0.25,
    // 0, 0, 0) interpolates 0.75·paper + 0.25·cyan corner =
    // (0.75, 0.9196, 0.9843).
    let doc = fixture_doc();
    let spaces = HashMap::new();
    let ctx = ResolutionContext::new(&doc, &spaces);
    let (r, g, b) = super::cmyk_to_rgb_via_intent(0.25, 0.0, 0.0, 0.0, &ctx);
    assert!((r - 0.75).abs() < 1e-4, "r: got {r}, want 0.75");
    assert!((g - 0.9196).abs() < 1e-4, "g: got {g}, want 0.9196");
    assert!((b - 0.9843).abs() < 1e-4, "b: got {b}, want 0.9843");
}

#[cfg(feature = "icc-qcms")]
#[test]
fn cmyk_to_rgb_via_intent_falls_back_when_profile_has_no_cmm() {
    // The header-only stub profile parses (IccProfile::parse accepts
    // the 128-byte header) but qcms refuses to build a Transform
    // from it because there's no tag table. The wrapper devolves to
    // its no-CMM fallback internally — the helper must agree with
    // the no-OutputIntent path on the same input. This is the
    // shape a real but malformed /OutputIntents profile would take.
    let doc = fixture_doc();
    let spaces = HashMap::new();
    let mut header_only = vec![0u8; 128];
    header_only[8..12].copy_from_slice(&0x04000000u32.to_be_bytes());
    header_only[12..16].copy_from_slice(b"prtr");
    header_only[16..20].copy_from_slice(b"CMYK");
    header_only[20..24].copy_from_slice(b"Lab ");
    header_only[36..40].copy_from_slice(b"acsp");
    let profile =
        std::sync::Arc::new(crate::color::IccProfile::parse(header_only, 4).expect("stub parses"));
    let ctx = ResolutionContext::new(&doc, &spaces).with_output_intent(Some(&profile));
    let (r, g, b) = super::cmyk_to_rgb_via_intent(0.25, 0.0, 0.0, 0.0, &ctx);
    // The no-CMM fallback of `convert_cmyk_pixel` routes through
    // `crate::extractors::images::cmyk_pixel_to_rgb`, which is now
    // the process-ink `crate::color::cmyk_to_rgb` — the same
    // conversion the no-OutputIntent arm takes. So both arms agree
    // on the process-ink value for CMYK(0.25,0,0,0) ≈
    // (0.75, 0.9196, 0.9843); the 8-bit CMM round-trip widens the
    // tolerance slightly.
    assert!((r - 0.75).abs() < 0.01, "got r={r}");
    assert!((g - 0.9196).abs() < 0.01, "got g={g}");
    assert!((b - 0.9843).abs() < 0.01, "got b={b}");
}
