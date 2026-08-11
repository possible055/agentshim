use super::*;

#[test]
fn image_mask_layout_rejects_non_positive_dimensions_before_allocation() {
    let negative = PageRenderer::image_mask_layout(&image_mask_dict(-1, 1));
    assert!(matches!(negative, Err(Error::Image(message)) if message.contains("positive")));

    let zero = PageRenderer::image_mask_layout(&image_mask_dict(1, 0));
    assert!(matches!(zero, Err(Error::Image(message)) if message.contains("positive")));
}

#[test]
fn image_mask_layout_accepts_large_valid_dimensions_without_allocating() {
    let layout =
        PageRenderer::image_mask_layout(&image_mask_dict(5_000, 4_000)).expect("valid layout");
    assert_eq!(layout, (5_000, 4_000, 625, 2_500_000, 80_000_000));
}

#[test]
fn ccitt_image_mask_rejects_width_beyond_decoder_limit() {
    let dict = ccitt_mask_dict(65_536, 1, 65_536, 1);
    let doc = minimal_pdf_doc();
    let result = PageRenderer::image_mask_ccitt_params(&dict, 65_536, 1, &doc);
    assert!(matches!(result, Err(Error::Image(message)) if message.contains("decoder limit")));
}

#[test]
fn ccitt_image_mask_rejects_dimension_mismatches() {
    let columns = ccitt_mask_dict(8, 1, 7, 1);
    let doc = minimal_pdf_doc();
    let result = PageRenderer::image_mask_ccitt_params(&columns, 8, 1, &doc);
    assert!(matches!(result, Err(Error::Image(message)) if message.contains("/Columns 7")));

    let rows = ccitt_mask_dict(8, 1, 8, 2);
    let result = PageRenderer::image_mask_ccitt_params(&rows, 8, 1, &doc);
    assert!(matches!(result, Err(Error::Image(message)) if message.contains("/Rows 2")));
}

#[test]
fn ccitt_image_mask_rejects_invalid_k_and_accepts_all_negative_k_values() {
    let doc = minimal_pdf_doc();
    let mut invalid = ccitt_mask_dict(8, 1, 8, 1);
    let Some(Object::Dictionary(invalid_params)) = invalid.get_mut("DecodeParms") else {
        panic!("decode parameters must be a dictionary");
    };
    invalid_params.insert("K".to_string(), Object::Name("invalid".to_string()));
    let result = PageRenderer::image_mask_ccitt_params(&invalid, 8, 1, &doc);
    assert!(
        matches!(result, Err(Error::Image(message)) if message.contains("/K must be an integer"))
    );

    let mut negative = ccitt_mask_dict(8, 1, 8, 1);
    let Some(Object::Dictionary(negative_params)) = negative.get_mut("DecodeParms") else {
        panic!("decode parameters must be a dictionary");
    };
    negative_params.insert("K".to_string(), Object::Integer(-2));
    let params = PageRenderer::image_mask_ccitt_params(&negative, 8, 1, &doc)
        .expect("valid parameters")
        .expect("CCITT parameters");
    assert!(params.is_group_4());
    assert_eq!(params.k, -2);
}

#[test]
fn ccitt_image_mask_rejects_dangling_and_wrong_type_decode_params_references() {
    let mut dangling = ccitt_mask_dict(8, 1, 8, 1);
    dangling.insert(
        "DecodeParms".to_string(),
        Object::Reference(ObjectRef::new(99, 0)),
    );
    let error = PageRenderer::image_mask_ccitt_params(&dangling, 8, 1, &minimal_pdf_doc())
        .expect_err("dangling reference must fail");
    assert!(
        matches!(&error, Error::Image(message) if message.contains("Unable to resolve")),
        "unexpected dangling-reference error: {error:?}"
    );

    let mut dangling_array = ccitt_mask_dict(8, 1, 8, 1);
    dangling_array.insert(
        "Filter".to_string(),
        Object::Array(vec![
            Object::Name("ASCIIHexDecode".to_string()),
            Object::Name("CCITTFaxDecode".to_string()),
        ]),
    );
    dangling_array.insert(
        "DecodeParms".to_string(),
        Object::Array(vec![Object::Null, Object::Reference(ObjectRef::new(99, 0))]),
    );
    let error = PageRenderer::image_mask_ccitt_params(&dangling_array, 8, 1, &minimal_pdf_doc())
        .expect_err("dangling array entry must fail");
    assert!(
        matches!(&error, Error::Image(message) if message.contains("array entry")),
        "unexpected dangling array-entry error: {error:?}"
    );

    let mut wrong_type = ccitt_mask_dict(8, 1, 8, 1);
    wrong_type.insert(
        "DecodeParms".to_string(),
        Object::Reference(ObjectRef::new(4, 0)),
    );
    let doc = pdf_doc_with_extra_object(Some(b"42"));
    let result = PageRenderer::image_mask_ccitt_params(&wrong_type, 8, 1, &doc);
    assert!(
        matches!(result, Err(Error::Image(message)) if message.contains("must be a dictionary"))
    );
}

#[test]
fn ccitt_image_mask_rejects_non_final_filter() {
    let mut dict = ccitt_mask_dict(8, 1, 8, 1);
    dict.insert(
        "Filter".to_string(),
        Object::Array(vec![
            Object::Name("CCITTFaxDecode".to_string()),
            Object::Name("ASCIIHexDecode".to_string()),
        ]),
    );
    let result = PageRenderer::image_mask_ccitt_params(&dict, 8, 1, &minimal_pdf_doc());
    assert!(matches!(result, Err(Error::Image(message)) if message.contains("final")));
}

#[test]
fn test_color_key_mask_range_logic() {
    // Two-component (e.g. grayscale-with-alpha style) sanity of the range
    // check: a component in [lo, hi] inclusive is masked.
    let ranges = [(10u32, 20u32), (100u32, 150u32)];

    // Both components in range -> masked (transparent).
    assert!(color_key_pixel_masked(&[15, 120], &ranges));
    // Boundaries are inclusive.
    assert!(color_key_pixel_masked(&[10, 100], &ranges));
    assert!(color_key_pixel_masked(&[20, 150], &ranges));
    // One component out of range -> not masked.
    assert!(!color_key_pixel_masked(&[9, 120], &ranges));
    assert!(!color_key_pixel_masked(&[15, 151], &ranges));
    // Length mismatch never masks.
    assert!(!color_key_pixel_masked(&[15], &ranges));
    assert!(!color_key_pixel_masked(&[], &ranges));

    // RGB color-key: only pixels equal to the exact keyed colour drop out.
    let rgb = [(255u32, 255u32), (0, 0), (0, 0)]; // pure red is transparent
    assert!(color_key_pixel_masked(&[255, 0, 0], &rgb));
    assert!(!color_key_pixel_masked(&[254, 0, 0], &rgb));
    assert!(!color_key_pixel_masked(&[255, 1, 0], &rgb));
}

#[test]
fn test_parse_color_key_mask() {
    // Well-formed 3-component array.
    let arr = vec![
        Object::Integer(0),
        Object::Integer(10),
        Object::Integer(20),
        Object::Integer(30),
        Object::Integer(40),
        Object::Integer(50),
    ];
    assert_eq!(
        parse_color_key_mask(&arr, 3),
        Some(vec![(0, 10), (20, 30), (40, 50)])
    );

    // Wrong length for ncomp -> None.
    assert_eq!(parse_color_key_mask(&arr, 2), None);
    // ncomp == 0 -> None.
    assert_eq!(parse_color_key_mask(&arr, 0), None);
    // min > max -> None.
    let bad = vec![Object::Integer(30), Object::Integer(10)];
    assert_eq!(parse_color_key_mask(&bad, 1), None);
    // Negative bound -> None.
    let neg = vec![Object::Integer(-1), Object::Integer(10)];
    assert_eq!(parse_color_key_mask(&neg, 1), None);
    // Non-integer entry -> None.
    let non_int = vec![Object::Real(1.5), Object::Integer(10)];
    assert_eq!(parse_color_key_mask(&non_int, 1), None);
}

#[test]
fn tiling_pattern_axis_tile_range_covers_region() {
    // A 10-px cell anchored at device x=0, stepping every 10 px, must
    // cover the region [5, 45] with tiles i = 0..=4.
    let (lo, hi) = axis_tile_range(5.0, 45.0, 0.0, 10.0, 10.0);
    assert!(lo <= 0, "lo {lo} should include tile 0");
    assert!(hi >= 4, "hi {hi} should include tile 4");
    for i in 0..=4 {
        assert!(lo <= i && i <= hi, "tile {i} must be in [{lo},{hi}]");
    }
    assert!(hi < 100 && lo > -100);
}

#[test]
fn tiling_pattern_axis_tile_range_negative_step() {
    // A flipped pattern axis (negative device step) must still yield a
    // valid, region-covering range.
    let (lo, hi) = axis_tile_range(0.0, 30.0, 0.0, 10.0, -10.0);
    assert!(lo <= hi);
    assert!(
        lo <= -3 && hi >= 0,
        "range [{lo},{hi}] must cover i in [-3,0]"
    );
}

#[test]
fn tiling_pattern_axis_tile_range_offset_anchor() {
    // Non-zero cell anchor: cell width 20, anchored at x=100, step 20,
    // region [130, 175] → tiles i=1,2,3 all included.
    let (lo, hi) = axis_tile_range(130.0, 175.0, 100.0, 20.0, 20.0);
    for i in 1..=3 {
        assert!(lo <= i && i <= hi, "tile {i} must be in [{lo},{hi}]");
    }
}

#[test]
fn test_cmyk_to_rgb_white() {
    let (r, g, b) = cmyk_to_rgb(0.0, 0.0, 0.0, 0.0);
    assert!((r - 1.0).abs() < 0.001);
    assert!((g - 1.0).abs() < 0.001);
    assert!((b - 1.0).abs() < 0.001);
}

#[test]
fn test_cmyk_to_rgb_black() {
    // Process inks (not additive): 100% K is the K ink #231F20, NOT #000000.
    let (r, g, b) = cmyk_to_rgb(0.0, 0.0, 0.0, 1.0);
    let q = |v: f32| (v * 255.0).round() as u8;
    assert_eq!([q(r), q(g), q(b)], [0x23, 0x1F, 0x20]);
}

#[test]
fn test_cmyk_to_rgb_pure_cyan() {
    // Process inks (not additive): 100% cyan is #00ADEF, NOT #00FFFF.
    let (r, g, b) = cmyk_to_rgb(1.0, 0.0, 0.0, 0.0);
    let q = |v: f32| (v * 255.0).round() as u8;
    assert_eq!([q(r), q(g), q(b)], [0x00, 0xAD, 0xEF]);
}

#[test]
fn test_negative_rect_normalization() {
    // Negative height: re 100 200 50 -30 → should normalize to (100, 170, 50, 30)
    let x: f32 = 100.0;
    let y: f32 = 200.0;
    let w: f32 = 50.0;
    let h: f32 = -30.0;
    let (nx, nw) = if w < 0.0 { (x + w, -w) } else { (x, w) };
    let (ny, nh) = if h < 0.0 { (y + h, -h) } else { (y, h) };
    assert!((nx - 100.0).abs() < 0.001);
    assert!((ny - 170.0).abs() < 0.001);
    assert!((nw - 50.0).abs() < 0.001);
    assert!((nh - 30.0).abs() < 0.001);
}

#[test]
fn test_negative_rect_both_negative() {
    let x: f32 = 100.0;
    let y: f32 = 200.0;
    let w: f32 = -50.0;
    let h: f32 = -30.0;
    let (nx, nw) = if w < 0.0 { (x + w, -w) } else { (x, w) };
    let (ny, nh) = if h < 0.0 { (y + h, -h) } else { (y, h) };
    assert!((nx - 50.0).abs() < 0.001);
    assert!((ny - 170.0).abs() < 0.001);
    assert!((nw - 50.0).abs() < 0.001);
    assert!((nh - 30.0).abs() < 0.001);
}

// -----------------------------------------------------------------
// WS1.5b — text render modes 4–7 "add to clip" (ISO 32000-1 §9.4.1 /
// §9.3.6 Table 106).
// -----------------------------------------------------------------

/// The crux of the ET application: the accumulated glyph silhouette is
/// converted to an alpha `Mask` and AND-ed into the current clip, so
/// subsequent content survives only where it falls inside BOTH the text
/// shape and the pre-existing clip. This exercises the exact
/// `Mask::from_pixmap(Alpha)` + `intersect_with_inherited` path the `ET`
/// arm runs, minus glyph shaping (which the coverage rasteriser handles).
#[test]
fn text_clip_intersects_glyph_silhouette_within_existing_clip() {
    use tiny_skia::{Color, FillRule, Mask, MaskType, Paint, PathBuilder, Pixmap, Rect, Transform};

    let w = 20u32;
    let h = 20u32;

    // Simulated accumulated text-clip silhouette: an opaque-black square
    // covering the page's centre (x,y in 5..15). This is what
    // `accumulate_text_clip_*` leaves in the scratch pixmap after a
    // mode-≥4 show.
    let mut scratch = Pixmap::new(w, h).unwrap();
    let mut paint = Paint::default();
    paint.set_color(Color::BLACK);
    paint.anti_alias = false;
    let sil = Rect::from_xywh(5.0, 5.0, 10.0, 10.0).unwrap();
    scratch.fill_rect(sil, &paint, Transform::identity(), None);

    // Degenerate guard: a silhouette WITH coverage reports true.
    let has_coverage = scratch.data().chunks_exact(4).any(|px| px[3] != 0);
    assert!(has_coverage, "painted silhouette must report coverage");

    // Existing clip: top half of the page (y in 0..10) fully inside.
    let mut existing = Mask::new(w, h).unwrap();
    let mut pb = PathBuilder::new();
    pb.push_rect(Rect::from_xywh(0.0, 0.0, 20.0, 10.0).unwrap());
    existing.fill_path(
        &pb.finish().unwrap(),
        FillRule::Winding,
        false,
        Transform::identity(),
    );

    // ET path: alpha mask from the silhouette, AND-ed with the clip.
    let text_mask = Mask::from_pixmap(scratch.as_ref(), MaskType::Alpha);
    let result = super::intersect_with_inherited(text_mask, Some(&existing));

    let at = |x: u32, y: u32| result.data()[(y * w + x) as usize];
    // Inside silhouette AND inside clip -> kept.
    assert_eq!(at(7, 7), 255, "kept where text ∩ clip");
    // Inside silhouette but BELOW the clip (y=12) -> removed by the clip.
    assert_eq!(at(7, 12), 0, "clip must not be widened past its bound");
    // Inside clip but OUTSIDE the silhouette (x=2) -> removed by the text.
    assert_eq!(
        at(2, 2),
        0,
        "content outside the glyph shape is clipped away"
    );
    // Corner outside both -> background.
    assert_eq!(at(18, 18), 0, "corner outside the glyph stays background");
}

/// An accumulator that saw only whitespace / outline-less glyphs is fully
/// transparent; the `ET` arm must treat that as degenerate and leave the
/// clip untouched rather than collapsing it to an empty region.
#[test]
fn text_clip_empty_accumulator_is_degenerate() {
    use tiny_skia::Pixmap;
    let scratch = Pixmap::new(16, 16).unwrap(); // fresh -> fully transparent
    let has_coverage = scratch.data().chunks_exact(4).any(|px| px[3] != 0);
    assert!(
        !has_coverage,
        "empty accumulator must be treated as no clip change"
    );
}

/// The coverage graphics state used to rasterise the clip silhouette must
/// force fill mode 0 (so clip-only mode-7 / invisible mode-3 glyphs still
/// rasterise their outline — the text rasteriser paints those modes with
/// transparent paint, which would otherwise yield an empty silhouette and
/// silently drop the clip) while forcing opaque paint (so alpha == coverage).
#[test]
fn coverage_gs_forces_fill_mode_for_clip_silhouette() {
    use crate::content::graphics_state::GraphicsState;
    for visible_mode in [3u8, 4, 5, 6, 7] {
        let mut gs = GraphicsState::default();
        gs.render_mode = visible_mode;
        gs.fill_alpha = 0.3;
        let cov = super::PageRenderer::coverage_only_gs(&gs);
        assert_eq!(
            cov.render_mode, 0,
            "coverage must fill regardless of visible mode {visible_mode}"
        );
        assert_eq!(cov.fill_alpha, 1.0, "coverage must be opaque");
        assert!(cov.smask.is_none(), "coverage must strip SMask");
    }
}
