use super::*;

// ---------------------------------------------------------------------
// Helper-level pins for the text-resolution splice.
//
// The text-side integration tests in
// `tests/test_render_resolution_pipeline_qa_wave*.rs` exercise the
// full renderer end-to-end, but two properties are not directly
// observable from there today:
//
//   * Stroke-side resolution. The text rasteriser does not currently
//     paint stroked glyphs, so the spliced stroke colour never reaches
//     the pixmap. We probe it here by inspecting the
//     `GraphicsState` the helper returns.
//
//   * Helper-returns-`None` on the no-op-splice path. The
//     integration test asserts the rendered output is unchanged when
//     the resolved RGBA equals the GS field already set, which holds
//     whether the helper returns `None` or `Some(clone)`. We probe
//     the return value directly here.
//
// Both probes call `pipeline_resolve_text_colors` directly. The
// wider integration coverage stays untouched.
// ---------------------------------------------------------------------

use crate::content::graphics_state::GraphicsState;
use crate::rendering::resolution::test_support::fixture_doc;
use smallvec::smallvec;
use std::collections::HashMap;

fn type4_magenta_separation_space() -> Object {
    // `{ 0.0 exch 0.0 0.0 }` — at full tint this yields CMYK(0,1,0,0),
    // which the colour resolver converts to RGB ≈ (1, 0, 1) (magenta).
    // Same shape as the colour-stage and pipeline regression tests.
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
    Object::Array(vec![
        Object::Name("Separation".into()),
        Object::Name("MagentaSpot".into()),
        Object::Name("DeviceCMYK".into()),
        func_obj,
    ])
}

#[test]
fn pipeline_resolve_text_colors_strokes_magenta_under_tr1() {
    // T-1 stroke-side resolution probe.
    //
    // Construct a `PageRenderer` with a Separation/DeviceCMYK/Type-4
    // colour space attached to the stroke side. Under Tr=1 the
    // helper must resolve the stroke side through the pipeline and
    // yield the Type-4-evaluated RGB on the `stroke` channel of the
    // returned `ResolvedColors`. The legacy `1.0 - tint = 0`
    // fallback would put black on the stroke channel; the pipeline
    // must produce magenta (R high, G low, B high).
    let mut renderer = PageRenderer::new(RenderOptions::default());
    renderer
        .color_spaces
        .insert("SpotMagenta".to_string(), type4_magenta_separation_space());

    let mut gs = GraphicsState::new();
    gs.render_mode = 1; // Stroke-only text.
    gs.stroke_color_space = "SpotMagenta".to_string();
    gs.stroke_color_components = smallvec![1.0]; // full tint
                                                 // Leave fill side at the GraphicsState default (DeviceGray, no
                                                 // components) so a stray fill-side resolve attempt would fail
                                                 // out — keeping the assertion focused on the stroke channel.

    let doc = fixture_doc();
    let colors = renderer
        .pipeline_resolve_text_colors(&doc, &gs)
        .expect("Tr=1 stroke side must produce ResolvedColors");

    let (r, g, b, _a) = colors.stroke.expect("Tr=1 must populate the stroke side");
    // Process-ink magenta corner #EC008C = (0.9255, 0, 0.5490); the
    // legacy 1-tint=0 fallback would put black on the stroke channel.
    assert!(
        (r - 0.9255).abs() < 0.02 && g < 0.02 && (b - 0.5490).abs() < 0.02,
        "stroke side must be process-ink magenta (Type-4 evaluated), \
         not the legacy 1-tint=0 black; got ({r}, {g}, {b})"
    );
    // The fill channel must not have been resolved — the helper
    // selects only the side(s) the Tr mode names.
    assert!(colors.fill.is_none(), "Tr=1 must not touch the fill side");
}

#[test]
fn pipeline_resolve_paint_gs_short_circuits_when_resolved_matches_gs() {
    // D-3 short-circuit. With a DeviceRGB fill already set on `gs`,
    // the pipeline resolves to the same (r, g, b, alpha) as
    // `gs.fill_color_rgb` / `gs.fill_alpha`. The helper must skip
    // the GraphicsState clone in that case and return `None` — the
    // caller borrows `gs` directly. This keeps the Device-family
    // path (the common case) allocation-free.
    let renderer = PageRenderer::new(RenderOptions::default());

    let mut gs = GraphicsState::new();
    gs.fill_color_space = "DeviceRGB".to_string();
    gs.fill_color_components = smallvec![0.25, 0.5, 0.75];
    // The dispatcher's inline path keeps `gs.fill_color_rgb` in
    // sync with the components; mirror that here so the
    // short-circuit comparison sees a true no-op.
    gs.fill_color_rgb = (0.25, 0.5, 0.75);
    gs.fill_alpha = 1.0;

    let doc = fixture_doc();
    assert!(
        renderer
            .pipeline_resolve_paint_gs(&doc, &gs, PipelinePaintKind::PathFill)
            .is_none(),
        "Device-family fill that resolves to the same RGBA as gs must short-circuit"
    );
}

#[test]
fn pipeline_resolve_paint_gs_image_mask_short_circuits_same_as_path_fill() {
    // Wave 3 pin. `PipelinePaintKind::ImageMask` must follow the
    // same fill-only resolve-and-short-circuit rules as
    // `PipelinePaintKind::PathFill`: a Device-family fill whose
    // resolved RGBA already matches `gs.fill_color_rgb` returns
    // `None` (no clone), and the stroke side is never touched.
    let renderer = PageRenderer::new(RenderOptions::default());

    let mut gs = GraphicsState::new();
    gs.fill_color_space = "DeviceRGB".to_string();
    gs.fill_color_components = smallvec![0.25, 0.5, 0.75];
    gs.fill_color_rgb = (0.25, 0.5, 0.75);
    gs.fill_alpha = 1.0;

    let doc = fixture_doc();
    assert!(
        renderer
            .pipeline_resolve_paint_gs(&doc, &gs, PipelinePaintKind::ImageMask)
            .is_none(),
        "ImageMask Device-family fill matching gs must short-circuit"
    );
}

#[test]
fn pipeline_resolve_paint_gs_image_mask_resolves_type4_separation_fill() {
    // ImageMask capability pin. With a Separation/DeviceCMYK Type 4
    // colour space on the fill side, the `ImageMask` variant must
    // produce a spliced `GraphicsState` whose `fill_color_rgb` is
    // the Type 4 program output (magenta), NOT the legacy
    // `1 - tint = 0` black. Same helper, same colour-stage path,
    // just driven by the ImageMask variant.
    let mut renderer = PageRenderer::new(RenderOptions::default());
    renderer
        .color_spaces
        .insert("SpotMagenta".to_string(), type4_magenta_separation_space());

    let mut gs = GraphicsState::new();
    gs.fill_color_space = "SpotMagenta".to_string();
    gs.fill_color_components = smallvec![1.0]; // full tint
    gs.fill_color_rgb = (0.0, 0.0, 0.0); // legacy 1-tint=0 black
    gs.fill_alpha = 1.0;

    let doc = fixture_doc();
    let spliced = renderer
        .pipeline_resolve_paint_gs(&doc, &gs, PipelinePaintKind::ImageMask)
        .expect("Type 4 Separation fill must splice through ImageMask variant");

    let (r, g, b) = spliced.fill_color_rgb;
    // Process-ink magenta corner #EC008C = (0.9255, 0, 0.5490).
    assert!(
        (r - 0.9255).abs() < 0.02 && g < 0.02 && (b - 0.5490).abs() < 0.02,
        "ImageMask fill must be process-ink magenta (Type 4 evaluated), not legacy black; got ({r}, {g}, {b})"
    );
    // Stroke side must remain untouched — the variant is fill-only.
    assert_eq!(
        spliced.stroke_color_rgb, gs.stroke_color_rgb,
        "ImageMask variant must not touch the stroke channel"
    );
}

#[test]
fn pipeline_resolve_text_colors_short_circuits_when_resolved_matches_gs() {
    // Same short-circuit on the text-side helper, Tr=0 fill-only:
    // a DeviceRGB whose resolved value equals the current gs fields
    // must produce no override (no per-element paint.set_color in
    // the rasteriser).
    let renderer = PageRenderer::new(RenderOptions::default());

    let mut gs = GraphicsState::new();
    gs.render_mode = 0;
    gs.fill_color_space = "DeviceRGB".to_string();
    gs.fill_color_components = smallvec![0.1, 0.2, 0.3];
    gs.fill_color_rgb = (0.1, 0.2, 0.3);
    gs.fill_alpha = 1.0;

    let doc = fixture_doc();
    assert!(
        renderer.pipeline_resolve_text_colors(&doc, &gs).is_none(),
        "Device-family text fill that resolves to the same RGBA as gs must short-circuit"
    );
}

#[test]
fn rgba_matches_within_epsilon() {
    // The tolerance must absorb single-ulp drift from intermediate
    // computations but reject any real colour change.
    assert!(rgba_matches((0.25, 0.5, 0.75, 1.0), (0.25, 0.5, 0.75), 1.0));
    // Sub-epsilon drift on every channel still matches.
    let drift = RGBA_MATCH_EPSILON * 0.5;
    assert!(rgba_matches(
        (0.25 + drift, 0.5 + drift, 0.75 + drift, 1.0 + drift),
        (0.25, 0.5, 0.75),
        1.0
    ));
    // Anything beyond the epsilon is a real change and must not
    // short-circuit — single-channel mismatch is enough.
    assert!(!rgba_matches(
        (0.26, 0.5, 0.75, 1.0),
        (0.25, 0.5, 0.75),
        1.0
    ));
    assert!(!rgba_matches(
        (0.25, 0.5, 0.75, 0.5),
        (0.25, 0.5, 0.75),
        1.0
    ));
}

// ---------------------------------------------------------------------
// `pipeline_resolve_components` helper unit pins.
//
// The shading integration tests in
// `tests/test_render_resolution_pipeline_qa_wave*.rs` probe the
// helper through the renderer. These unit pins probe the helper's
// own contract directly, so a regression in routing (e.g.
// Device-family short-circuit vs Spaced dispatch) shows up at the
// helper level before any pixel-comparison machinery is involved.
// ---------------------------------------------------------------------

#[test]
fn pipeline_resolve_components_resolves_type4_separation_to_correct_rgba() {
    // Capability pin. The Separation/DeviceCMYK/Type-4 space at
    // full tint must come out as magenta after the pipeline runs
    // the PostScript program — the same regression case the
    // colour-stage and full-pipeline unit tests pin at lower
    // levels, here verified via the wave-4 shading-endpoint
    // overload.
    let renderer = PageRenderer::new(RenderOptions::default());

    let space = type4_magenta_separation_space();
    let doc = fixture_doc();
    let color_spaces: HashMap<String, Object> = HashMap::new();

    let rgba = renderer
        .pipeline_resolve_components(&doc, &color_spaces, &space, &[1.0], 1.0)
        .expect("Type 4 Separation full-tint must resolve to Some(rgba)");
    let (r, g, b, a) = rgba;
    assert!(
        (r - 0.9255).abs() < 1.0e-3
            && g.abs() < 1.0e-3
            && (b - 0.5490).abs() < 1.0e-3
            && (a - 1.0).abs() < 1.0e-3,
        "Type 4 Separation at tint=1 must produce process-ink magenta RGBA \
         (#EC008C ≈ 0.9255, 0, 0.5490, 1); got ({r}, {g}, {b}, {a})"
    );
}

#[test]
fn pipeline_resolve_components_short_circuits_for_device_families() {
    // Parity pin. For DeviceRGB / DeviceGray / DeviceCMYK the
    // pipeline must produce the same RGBA the inline shading
    // path would compute (modulo the inline path's
    // long-standing DeviceCMYK truncation bug, which is the
    // entire reason wave 4 exists). The pin here is on the
    // resolver's behaviour, not on the inline path: for each
    // device family the resolved RGBA must equal the
    // mathematically-correct device→RGB conversion.
    let renderer = PageRenderer::new(RenderOptions::default());
    let doc = fixture_doc();
    let color_spaces: HashMap<String, Object> = HashMap::new();

    // DeviceRGB: components pass through verbatim.
    let rgb_space = Object::Name("DeviceRGB".to_string());
    let rgba = renderer
        .pipeline_resolve_components(&doc, &color_spaces, &rgb_space, &[0.5, 0.25, 0.75], 0.8)
        .expect("DeviceRGB must resolve");
    let (r, g, b, a) = rgba;
    assert!(
        (r - 0.5).abs() < 1.0e-6
            && (g - 0.25).abs() < 1.0e-6
            && (b - 0.75).abs() < 1.0e-6
            && (a - 0.8).abs() < 1.0e-6,
        "DeviceRGB must pass components through verbatim with alpha folded; got ({r}, {g}, {b}, {a})"
    );

    // DeviceGray: single component expanded to (g, g, g).
    let gray_space = Object::Name("DeviceGray".to_string());
    let rgba = renderer
        .pipeline_resolve_components(&doc, &color_spaces, &gray_space, &[0.42], 1.0)
        .expect("DeviceGray must resolve");
    let (r, g, b, _a) = rgba;
    assert!(
        (r - 0.42).abs() < 1.0e-6 && (g - 0.42).abs() < 1.0e-6 && (b - 0.42).abs() < 1.0e-6,
        "DeviceGray must expand the single component to (g, g, g); got ({r}, {g}, {b})"
    );

    // DeviceCMYK: process-ink conversion (tetralinear over the 16
    // measured ink corners). Pure cyan (1, 0, 0, 0) lands on the
    // measured cyan corner #00ADEF = (0, 0.6784, 0.9373).
    let cmyk_space = Object::Name("DeviceCMYK".to_string());
    let rgba = renderer
        .pipeline_resolve_components(&doc, &color_spaces, &cmyk_space, &[1.0, 0.0, 0.0, 0.0], 1.0)
        .expect("DeviceCMYK must resolve");
    let (r, g, b, _a) = rgba;
    assert!(
        r.abs() < 1.0e-3 && (g - 0.6784).abs() < 1.0e-3 && (b - 0.9373).abs() < 1.0e-3,
        "DeviceCMYK pure cyan must map to process-ink #00ADEF (0, 0.6784, 0.9373); got ({r}, {g}, {b})"
    );
}

// Perf-regression probe: apply_pending_clip's only expensive work is the
// path-rasterization branch that runs when `pending_clip` is Some. A naive
// refactor (e.g. dropping the `Option::take` short-circuit, or treating
// every paint op as a fresh clip) would explode the materialization count
// to O(paint ops). This test pins the contract by driving the function
// directly with K paint-op-style invocations and N clip-state changes, and
// asserting the materialization count equals N — not K.
//
// The probe is serialized via `APC_PROBE_LOCK` because it reads / resets a
// process-wide AtomicU64. No other test in this mod calls
// `apply_pending_clip`, but the lock keeps the contract safe under future
// additions and under `cargo test -- --test-threads=1` parity.
static APC_PROBE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn apply_pending_clip_materializes_only_per_clip_state_change() {
    use crate::content::GraphicsStateStack;
    use std::sync::atomic::Ordering;
    use tiny_skia::{FillRule, PathBuilder, Pixmap, Rect, Transform};

    let _guard = APC_PROBE_LOCK.lock().unwrap();

    let pixmap = Pixmap::new(200, 200).expect("pixmap");
    let gs_stack = GraphicsStateStack::new();
    let base_transform = Transform::identity();

    let make_clip_path = || {
        let mut pb = PathBuilder::new();
        pb.push_rect(Rect::from_xywh(10.0, 10.0, 50.0, 50.0).unwrap());
        pb.finish().unwrap()
    };

    // Scenario A: 1 clip-state change followed by K paint-op-style calls
    // with no pending clip. Only the first call should materialize.
    const K: usize = 100;
    APC_MATERIALIZED.store(0, Ordering::Relaxed);
    let mut clip_stack: Vec<Option<tiny_skia::Mask>> = vec![None];
    let mut pending: Option<(tiny_skia::Path, FillRule)> =
        Some((make_clip_path(), FillRule::Winding));
    for _ in 0..K {
        apply_pending_clip(
            &mut pending,
            &mut clip_stack,
            &pixmap,
            base_transform,
            &gs_stack,
        );
    }
    let after_one_clip = APC_MATERIALIZED.load(Ordering::Relaxed);
    assert_eq!(
        after_one_clip, 1,
        "1 W operator followed by {K} paint ops must materialize the clip \
         mask exactly once (got {after_one_clip})"
    );

    // Scenario B: N clip-state changes each followed by K paint ops.
    // Materialization count must equal N, not K*N.
    const N: usize = 5;
    APC_MATERIALIZED.store(0, Ordering::Relaxed);
    let mut clip_stack: Vec<Option<tiny_skia::Mask>> = vec![None];
    for _ in 0..N {
        let mut pending: Option<(tiny_skia::Path, FillRule)> =
            Some((make_clip_path(), FillRule::Winding));
        for _ in 0..K {
            apply_pending_clip(
                &mut pending,
                &mut clip_stack,
                &pixmap,
                base_transform,
                &gs_stack,
            );
        }
    }
    let after_n_clips = APC_MATERIALIZED.load(Ordering::Relaxed);
    assert_eq!(
        after_n_clips, N as u64,
        "{N} W operators each followed by {K} paint ops must materialize \
         exactly {N} times (got {after_n_clips})"
    );
}

/// `type3_font_matrix` returns the explicit `/FontMatrix` when well-formed,
/// and falls back to the Type 1 default for missing / malformed entries.
#[test]
fn type3_font_matrix_parse() {
    // Explicit, well-formed matrix is honoured.
    let mut d: HashMap<String, Object> = HashMap::new();
    d.insert(
        "FontMatrix".into(),
        Object::Array(vec![
            Object::Real(0.01),
            Object::Integer(0),
            Object::Integer(0),
            Object::Real(0.02),
            Object::Integer(5),
            Object::Integer(6),
        ]),
    );
    let m = type3_font_matrix(&d);
    assert!((m.sx - 0.01).abs() < 1e-9 && (m.sy - 0.02).abs() < 1e-9);
    assert!((m.tx - 5.0).abs() < 1e-6 && (m.ty - 6.0).abs() < 1e-6);

    // Missing entry → 1/1000 default.
    let empty: HashMap<String, Object> = HashMap::new();
    let def = type3_font_matrix(&empty);
    assert!((def.sx - 0.001).abs() < 1e-9 && (def.sy - 0.001).abs() < 1e-9);

    // Wrong arity → default.
    let mut bad: HashMap<String, Object> = HashMap::new();
    bad.insert("FontMatrix".into(), Object::Array(vec![Object::Real(0.5)]));
    let badm = type3_font_matrix(&bad);
    assert!((badm.sx - 0.001).abs() < 1e-9);
}

/// Build a minimal single-page PDF with a Type 3 font whose only glyph
/// (`/rect`, code 65) is a `d1` stencil that fills a 700×700 glyph-space
/// rectangle. The page shows it once, at font size 100, after setting the
/// fill colour to red.
fn build_type3_rect_pdf() -> Vec<u8> {
    let mut pdf = Vec::new();
    let mut offsets: Vec<usize> = Vec::new();
    pdf.extend_from_slice(b"%PDF-1.4\n");

    offsets.push(pdf.len());
    pdf.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n\n");

    offsets.push(pdf.len());
    pdf.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n\n");

    offsets.push(pdf.len());
    pdf.extend_from_slice(
        b"3 0 obj\n\
          << /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100]\n\
             /Contents 4 0 R\n\
             /Resources << /Font << /T3 5 0 R >> >>\n\
          >>\nendobj\n\n",
    );

    // Page content: red fill, then show code 65 at size 100 near (10,10).
    let content = b"BT /T3 100 Tf 1 0 0 rg 10 10 Td (A) Tj ET";
    offsets.push(pdf.len());
    let hdr = format!("4 0 obj\n<< /Length {} >>\nstream\n", content.len());
    pdf.extend_from_slice(hdr.as_bytes());
    pdf.extend_from_slice(content);
    pdf.extend_from_slice(b"\nendstream\nendobj\n\n");

    // Type 3 font dictionary.
    offsets.push(pdf.len());
    pdf.extend_from_slice(
        b"5 0 obj\n\
          << /Type /Font /Subtype /Type3 /FontBBox [0 0 750 750]\n\
             /FontMatrix [0.001 0 0 0.001 0 0]\n\
             /FirstChar 65 /LastChar 65 /Widths [700]\n\
             /Encoding 6 0 R /CharProcs 7 0 R >>\nendobj\n\n",
    );

    // Encoding: code 65 → glyph name /rect.
    offsets.push(pdf.len());
    pdf.extend_from_slice(b"6 0 obj\n<< /Type /Encoding /Differences [65 /rect] >>\nendobj\n\n");

    // CharProcs: /rect → glyph stream 8.
    offsets.push(pdf.len());
    pdf.extend_from_slice(b"7 0 obj\n<< /rect 8 0 R >>\nendobj\n\n");

    // Glyph description: d1 stencil filling a 700×700 glyph-space rect.
    let glyph = b"700 0 0 0 700 700 d1 0 0 700 700 re f";
    offsets.push(pdf.len());
    let ghdr = format!("8 0 obj\n<< /Length {} >>\nstream\n", glyph.len());
    pdf.extend_from_slice(ghdr.as_bytes());
    pdf.extend_from_slice(glyph);
    pdf.extend_from_slice(b"\nendstream\nendobj\n\n");

    let xref_offset = pdf.len();
    let n_obj = offsets.len() + 1;
    let mut xref = format!("xref\n0 {}\n", n_obj);
    xref.push_str("0000000000 65535 f \n");
    for off in &offsets {
        xref.push_str(&format!("{:010} 00000 n \n", off));
    }
    pdf.extend_from_slice(xref.as_bytes());
    let trailer = format!(
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
        n_obj, xref_offset
    );
    pdf.extend_from_slice(trailer.as_bytes());
    pdf
}

/// The Type 3 `d1` glyph paints a filled rectangle that takes the current
/// (red) fill colour, producing non-blank red pixels in the glyph cell.
#[test]
fn type3_d1_glyph_renders_filled_rect() {
    use crate::document::PdfDocument;

    let pdf = build_type3_rect_pdf();
    let doc = PdfDocument::from_bytes(pdf).expect("parse Type3 PDF");

    let opts = RenderOptions {
        format: ImageFormat::RawRgba8,
        ..RenderOptions::with_dpi(150)
    };
    let mut renderer = PageRenderer::new(opts);
    let img = renderer.render_page(&doc, 0).expect("render page");

    assert_eq!(img.format, ImageFormat::RawRgba8);
    assert_eq!(img.data.len(), (img.width * img.height * 4) as usize);

    // Count red pixels: R high, G/B low. A blank page (glyph not painted)
    // yields zero; the d1 stencil taking the current fill colour yields a
    // solid red rectangle.
    let mut red = 0usize;
    for px in img.data.chunks_exact(4) {
        if px[0] > 200 && px[1] < 80 && px[2] < 80 {
            red += 1;
        }
    }
    assert!(
        red > 200,
        "expected a red Type3 d1 glyph rectangle, found {red} red pixels \
         in a {}x{} image",
        img.width,
        img.height
    );
}
