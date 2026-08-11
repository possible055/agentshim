use super::*;

#[test]
fn rgb_to_cmyk_round_trips_in_gamut_and_stays_in_range() {
    let q = |v: f32| (v * 255.0).round() as i32;
    // Any colour reachable by a K=0 CMY mix must round-trip
    // RGB -> CMYK -> RGB to within a couple of 8-bit steps.
    for &(c0, m0, y0) in &[
        (0.0, 0.0, 0.0),
        (1.0, 0.0, 0.0),
        (0.0, 1.0, 0.0),
        (0.0, 0.0, 1.0),
        (0.5, 0.2, 0.0),
        (0.3, 0.3, 0.3),
        (0.8, 0.4, 0.1),
        (1.0, 1.0, 1.0),
    ] {
        let (r, g, b) = cmyk_to_rgb(c0, m0, y0, 0.0);
        let (c, m, y, k) = rgb_to_cmyk(r, g, b);
        assert_eq!(k, 0.0, "separation uses K=0");
        let (rr, gg, bb) = cmyk_to_rgb(c, m, y, k);
        for (got, want) in [(rr, r), (gg, g), (bb, b)] {
            assert!(
                    (q(got) - q(want)).abs() <= 3,
                    "in-gamut round-trip off: cmy ({c0},{m0},{y0}) rgb ({r},{g},{b}) -> ({rr},{gg},{bb})"
                );
        }
    }
    // Out-of-gamut sRGB primary blue cannot be reproduced by any process CMY
    // mix; it must map to a valid in-range CMYK (nearest in-gamut), not diverge.
    let (c, m, y, k) = rgb_to_cmyk(0.0, 0.0, 1.0);
    for v in [c, m, y, k] {
        assert!((0.0..=1.0).contains(&v), "out-of-gamut CMYK stays in range");
    }
}

#[test]
fn cmyk_uses_process_inks_not_the_naive_complement() {
    let q = |v: f32| (v * 255.0).round() as u8;
    let rgb = |c, m, y, k| {
        let (r, g, b) = cmyk_to_rgb(c, m, y, k);
        [q(r), q(g), q(b)]
    };
    // K ink is #231F20, NOT #000000 - the case that matters most, since print
    // PDFs set body text with `0 0 0 1 k`.
    assert_eq!(rgb(0.0, 0.0, 0.0, 1.0), [35, 31, 32]);
    // Process cyan / magenta / yellow.
    assert_eq!(rgb(1.0, 0.0, 0.0, 0.0), [0, 173, 239]);
    assert_eq!(rgb(0.0, 1.0, 0.0, 0.0), [236, 0, 140]);
    assert_eq!(rgb(0.0, 0.0, 1.0, 0.0), [255, 242, 0]);
    // Paper and registration are still the extremes.
    assert_eq!(rgb(0.0, 0.0, 0.0, 0.0), [255, 255, 255]);
    assert_eq!(rgb(1.0, 1.0, 1.0, 1.0), [0, 0, 0]);
    // An interior mix interpolates.
    assert_eq!(rgb(0.669, 0.0, 0.381, 0.0), [84, 197, 172]);
}

/// Minimal valid ICC header — just enough to satisfy `parse`.
/// Bytes 0-3: size; 4-7: CMM; 8-11: version (4.2.0.0); 12-15: devClass;
/// 16-19: colour space; 20-23: PCS; … 36-39: 'acsp'. Remaining bytes
/// unused for this test.
fn minimal_header(cs: &[u8; 4], n_bytes: usize) -> Vec<u8> {
    let mut v = vec![0u8; n_bytes.max(128)];
    v[8..12].copy_from_slice(&0x04200000u32.to_be_bytes());
    v[12..16].copy_from_slice(b"prtr");
    v[16..20].copy_from_slice(cs);
    v[20..24].copy_from_slice(b"Lab ");
    v[36..40].copy_from_slice(b"acsp");
    v
}

#[test]
fn header_parse_requires_acsp_signature() {
    let mut bytes = minimal_header(b"CMYK", 128);
    bytes[36..40].copy_from_slice(b"xxxx");
    assert!(IccHeader::parse(&bytes).is_none());
}

#[test]
fn header_parse_rejects_short_input() {
    let bytes = vec![0u8; 127];
    assert!(IccHeader::parse(&bytes).is_none());
}

#[test]
fn header_identifies_cmyk_as_4_components() {
    let bytes = minimal_header(b"CMYK", 128);
    let h = IccHeader::parse(&bytes).expect("valid header");
    assert_eq!(h.input_components(), Some(4));
    assert_eq!(&h.color_space, b"CMYK");
    assert_eq!(&h.device_class, b"prtr");
}

#[test]
fn profile_parse_rejects_n_mismatch() {
    // Header advertises CMYK (4 components) but dictionary declares N=3.
    // PDF §8.6.5.5 requires these to agree.
    let bytes = minimal_header(b"CMYK", 128);
    assert!(IccProfile::parse(bytes, 3).is_none());
}

#[test]
fn profile_parse_accepts_matching_n() {
    let bytes = minimal_header(b"CMYK", 128);
    let p = IccProfile::parse(bytes, 4).expect("should parse");
    assert_eq!(p.n_components(), 4);
}

#[test]
fn intent_default_is_relative_colorimetric() {
    assert_eq!(
        RenderingIntent::default(),
        RenderingIntent::RelativeColorimetric
    );
}

#[test]
fn intent_from_pdf_name_falls_back_to_relative_colorimetric() {
    // §8.6.5.8: unrecognized names fall through.
    assert_eq!(
        RenderingIntent::from_pdf_name("WhateverNotReal"),
        RenderingIntent::RelativeColorimetric,
    );
    assert_eq!(
        RenderingIntent::from_pdf_name("Perceptual"),
        RenderingIntent::Perceptual,
    );
    assert_eq!(
        RenderingIntent::from_pdf_name("Saturation"),
        RenderingIntent::Saturation,
    );
    assert_eq!(
        RenderingIntent::from_pdf_name("AbsoluteColorimetric"),
        RenderingIntent::AbsoluteColorimetric,
    );
}

#[test]
fn phase1_transform_preserves_srgb_white() {
    let bytes = minimal_header(b"CMYK", 128);
    let p = Arc::new(IccProfile::parse(bytes, 4).unwrap());
    let t = Transform::new_srgb_target(p, RenderingIntent::RelativeColorimetric);
    // CMYK(0,0,0,0) → sRGB white under any sensible transform.
    assert_eq!(t.convert_cmyk_pixel(0, 0, 0, 0), [255, 255, 255]);
    // CMYK(255,255,255,255) → sRGB black under the §10.3.5 fallback.
    assert_eq!(t.convert_cmyk_pixel(255, 255, 255, 255), [0, 0, 0]);
}

#[test]
fn active_backend_retarget_capability_matches_feature() {
    assert!(!active_backend_supports_cmyk_retarget());
}
