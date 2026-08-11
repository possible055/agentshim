use super::*;
use smallvec::{smallvec, SmallVec};

use super::super::intent::{PaintKind, PaintSide};
use super::super::resolved::{
    BlendPlan, ClipPlan, InkSelector, OverprintPlan, ParticipatingChannel, ResolvedColor,
    ResolvedPaintCmd,
};

fn rect_path() -> tiny_skia::Path {
    let mut pb = tiny_skia::PathBuilder::new();
    pb.move_to(0.0, 0.0);
    pb.line_to(10.0, 0.0);
    pb.line_to(10.0, 10.0);
    pb.line_to(0.0, 10.0);
    pb.close();
    pb.finish().expect("non-empty path")
}

fn fresh_pixmap() -> Pixmap {
    Pixmap::new(16, 16).expect("16x16 pixmap allocates")
}

fn cmyk_cmd<'a>(path: &'a tiny_skia::Path, c: f32, m: f32, y: f32, k: f32) -> ResolvedPaintCmd<'a> {
    ResolvedPaintCmd {
        kind: PaintKind::Path {
            path,
            fill_rule: FillRule::Winding,
        },
        side: PaintSide::Fill,
        color: ResolvedColor::Cmyk { c, m, y, k, a: 1.0 },
        overprint: OverprintPlan {
            enabled: false,
            mode: 0,
            participating: smallvec![
                ParticipatingChannel {
                    ink: InkName::new("Cyan"),
                    value: c,
                },
                ParticipatingChannel {
                    ink: InkName::new("Magenta"),
                    value: m,
                },
                ParticipatingChannel {
                    ink: InkName::new("Yellow"),
                    value: y,
                },
                ParticipatingChannel {
                    ink: InkName::new("Black"),
                    value: k,
                },
            ],
            selector: InkSelector::Listed,
            all_tint: 0.0,
            spot_source: None,
            alt_cmyk_fallback: None,
        },
        blend: BlendPlan::Native(tiny_skia::BlendMode::SourceOver),
        clip: ClipPlan::None,
        ctm: Matrix::identity(),
    }
}

#[test]
fn fill_routes_cmyk_to_matching_plates() {
    // A DeviceCMYK fill at (0.5, 0.25, 0.0, 1.0) paints the Cyan,
    // Magenta, and Black plates at the respective tints. Yellow gets
    // 0.0 (knock-out under default OP=false), painted as a zero-tint
    // rectangle. This is the per-plate routing the existing inline
    // renderer's tint_for_ink performs — now driven via the pipeline.
    let path = rect_path();
    let cmd = cmyk_cmd(&path, 0.5, 0.25, 0.0, 1.0);
    let mut plates = vec![
        fresh_pixmap(),
        fresh_pixmap(),
        fresh_pixmap(),
        fresh_pixmap(),
    ];
    let inks = [
        InkName::new("Cyan"),
        InkName::new("Magenta"),
        InkName::new("Yellow"),
        InkName::new("Black"),
    ];
    let surface = SeparationSurface {
        pixmaps: &mut plates,
        inks: &inks,
        base_transform: Transform::identity(),
    };
    let mut backend = SeparationBackend::new();
    backend.paint(&cmd, surface).unwrap();

    // Sample pixel (5, 5), which sits inside the 10x10 rect. The
    // R channel of each plate carries the per-ink tint.
    let sample = |p: &Pixmap| p.data()[(5 * 16 + 5) * 4];
    assert_eq!(sample(&plates[0]), 128, "Cyan tint ≈ 0.5");
    assert_eq!(sample(&plates[1]), 64, "Magenta tint ≈ 0.25");
    // Yellow under default OP=false: painted with 0.0 (knock-out).
    // The plate was zero before; painting zero leaves it zero.
    assert_eq!(sample(&plates[2]), 0, "Yellow tint = 0.0 knock-out");
    assert_eq!(sample(&plates[3]), 255, "Black tint = 1.0 full ink");
}

#[test]
fn fill_skips_spot_plates_when_overprint_enabled() {
    // §11.7.4 with OP=true: the spot plate (not named by the source)
    // is left untouched. We pre-fill it with a sentinel to verify
    // it's not overwritten.
    let path = rect_path();
    let mut cmd = cmyk_cmd(&path, 0.5, 0.0, 0.0, 0.0);
    cmd.overprint.enabled = true;
    let mut plates = vec![fresh_pixmap(), fresh_pixmap()];
    // Pre-fill the spot plate with red so we can detect overwrites.
    let sentinel = tiny_skia::Color::from_rgba8(200, 0, 0, 255);
    let mut spot_paint = tiny_skia::Paint::default();
    spot_paint.set_color(sentinel);
    let full_rect = tiny_skia::Rect::from_xywh(0.0, 0.0, 16.0, 16.0).unwrap();
    plates[1].fill_path(
        &tiny_skia::PathBuilder::from_rect(full_rect),
        &spot_paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );
    let inks = [InkName::new("Cyan"), InkName::new("PANTONE 185 C")];
    let surface = SeparationSurface {
        pixmaps: &mut plates,
        inks: &inks,
        base_transform: Transform::identity(),
    };
    let mut backend = SeparationBackend::new();
    backend.paint(&cmd, surface).unwrap();

    // Cyan painted with tint 0.5 -> 128.
    assert_eq!(plates[0].data()[(5 * 16 + 5) * 4], 128);
    // Spot plate untouched -> sentinel R=200 still visible.
    assert_eq!(plates[1].data()[(5 * 16 + 5) * 4], 200);
}

#[test]
fn per_channel_devicen_routes_named_plates() {
    // DeviceN with named channels: each plate paints from the
    // channel matching its ink name. The PerChannel variant is the
    // separation-side colour the pipeline produces for DeviceN
    // sources (once the resolver grows the backend-aware shape;
    // today this test constructs it directly).
    let path = rect_path();
    let cmd = ResolvedPaintCmd {
        kind: PaintKind::Path {
            path: &path,
            fill_rule: FillRule::Winding,
        },
        side: PaintSide::Fill,
        color: ResolvedColor::PerChannel {
            channels: Box::new(smallvec![
                (InkName::new("PANTONE 185 C"), 0.75),
                (InkName::new("Dieline"), 0.1),
            ]),
            a: 1.0,
        },
        overprint: OverprintPlan {
            enabled: false,
            mode: 0,
            participating: smallvec![
                ParticipatingChannel {
                    ink: InkName::new("PANTONE 185 C"),
                    value: 0.75,
                },
                ParticipatingChannel {
                    ink: InkName::new("Dieline"),
                    value: 0.1,
                },
            ],
            selector: InkSelector::Listed,
            all_tint: 0.0,
            spot_source: None,
            alt_cmyk_fallback: None,
        },
        blend: BlendPlan::Native(tiny_skia::BlendMode::SourceOver),
        clip: ClipPlan::None,
        ctm: Matrix::identity(),
    };
    let mut plates = vec![fresh_pixmap(), fresh_pixmap()];
    let inks = [InkName::new("PANTONE 185 C"), InkName::new("Dieline")];
    let surface = SeparationSurface {
        pixmaps: &mut plates,
        inks: &inks,
        base_transform: Transform::identity(),
    };
    let mut backend = SeparationBackend::new();
    backend.paint(&cmd, surface).unwrap();
    // 0.75 -> 191 (round half away from zero), 0.1 -> 26.
    assert_eq!(plates[0].data()[(5 * 16 + 5) * 4], 191);
    assert_eq!(plates[1].data()[(5 * 16 + 5) * 4], 26);
}

#[test]
fn rgb_color_routes_to_no_plates() {
    // §11.7.4: RGB sources don't route to plates. The router yields
    // Skip for every plate, so every plate stays untouched.
    let path = rect_path();
    let cmd = ResolvedPaintCmd {
        kind: PaintKind::Path {
            path: &path,
            fill_rule: FillRule::Winding,
        },
        side: PaintSide::Fill,
        color: ResolvedColor::Rgba {
            r: 1.0,
            g: 0.0,
            b: 0.0,
            a: 1.0,
        },
        // OverprintResolver produces empty participating for RGB.
        overprint: OverprintPlan {
            enabled: false,
            mode: 0,
            participating: SmallVec::new(),
            selector: InkSelector::Listed,
            all_tint: 0.0,
            spot_source: None,
            alt_cmyk_fallback: None,
        },
        blend: BlendPlan::Native(tiny_skia::BlendMode::SourceOver),
        clip: ClipPlan::None,
        ctm: Matrix::identity(),
    };
    let mut plates = vec![fresh_pixmap()];
    let inks = [InkName::new("Cyan")];
    let surface = SeparationSurface {
        pixmaps: &mut plates,
        inks: &inks,
        base_transform: Transform::identity(),
    };
    let mut backend = SeparationBackend::new();
    backend.paint(&cmd, surface).unwrap();
    // Plate untouched.
    assert_eq!(plates[0].data()[(5 * 16 + 5) * 4], 0);
}

#[test]
fn opm1_zero_component_on_cmyk_skips_matching_plate() {
    // §11.7.4.3 OPM=1 Adobe nonzero overprint: a zero source
    // component on DeviceCMYK skips that plate even when overprint
    // is enabled. Pre-fill Magenta with sentinel to verify.
    let path = rect_path();
    let mut cmd = cmyk_cmd(&path, 0.5, 0.0, 0.0, 0.0);
    cmd.overprint.enabled = true;
    cmd.overprint.mode = 1;
    let mut plates = vec![fresh_pixmap(), fresh_pixmap()];
    // Pre-fill Magenta plate with sentinel.
    let sentinel = tiny_skia::Color::from_rgba8(99, 0, 0, 255);
    let mut spot_paint = tiny_skia::Paint::default();
    spot_paint.set_color(sentinel);
    let full_rect = tiny_skia::Rect::from_xywh(0.0, 0.0, 16.0, 16.0).unwrap();
    plates[1].fill_path(
        &tiny_skia::PathBuilder::from_rect(full_rect),
        &spot_paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );
    let inks = [InkName::new("Cyan"), InkName::new("Magenta")];
    let surface = SeparationSurface {
        pixmaps: &mut plates,
        inks: &inks,
        base_transform: Transform::identity(),
    };
    let mut backend = SeparationBackend::new();
    backend.paint(&cmd, surface).unwrap();
    // Cyan painted at 0.5 -> 128.
    assert_eq!(plates[0].data()[(5 * 16 + 5) * 4], 128);
    // Magenta untouched under OPM=1 (zero source component).
    assert_eq!(plates[1].data()[(5 * 16 + 5) * 4], 99);
}

#[test]
fn color_only_intent_paints_nothing() {
    // ColorOnly intents carry no geometry — the backend must not
    // attempt to rasterise anything.
    let cmd = ResolvedPaintCmd {
        kind: PaintKind::ColorOnly,
        side: PaintSide::Fill,
        color: ResolvedColor::Cmyk {
            c: 1.0,
            m: 0.0,
            y: 0.0,
            k: 0.0,
            a: 1.0,
        },
        overprint: OverprintPlan {
            enabled: false,
            mode: 0,
            participating: smallvec![ParticipatingChannel {
                ink: InkName::new("Cyan"),
                value: 1.0,
            }],
            selector: InkSelector::Listed,
            all_tint: 0.0,
            spot_source: None,
            alt_cmyk_fallback: None,
        },
        blend: BlendPlan::Native(tiny_skia::BlendMode::SourceOver),
        clip: ClipPlan::None,
        ctm: Matrix::identity(),
    };
    let mut plates = vec![fresh_pixmap()];
    let inks = [InkName::new("Cyan")];
    let surface = SeparationSurface {
        pixmaps: &mut plates,
        inks: &inks,
        base_transform: Transform::identity(),
    };
    let mut backend = SeparationBackend::new();
    backend.paint(&cmd, surface).unwrap();
    // No geometry painted -> plate stays at zero.
    assert_eq!(plates[0].data()[(5 * 16 + 5) * 4], 0);
}

/// Drive the per-plate fill through `SeparationBackend::fill_plate` and
/// in parallel through `separation_renderer::fill_separation`, then
/// assert each plate's pixel buffer matches byte-for-byte.
///
/// `inks` and `tints` are parallel slices: `tints[i]` is the value the
/// backend would have routed to `inks[i]`. The caller computes them so
/// the test specifies exactly what the comparison reference is, instead
/// of trusting an internal copy of the routing logic.
fn assert_backend_matches_inline(
    path: &tiny_skia::Path,
    ctm: Matrix,
    cmd: ResolvedPaintCmd<'_>,
    inks: &[InkName],
    tints: &[f32],
    fill_rule: FillRule,
) {
    assert_eq!(inks.len(), tints.len());
    // Backend route: call into the real public `paint` API.
    let mut backend_plates: Vec<Pixmap> = (0..inks.len()).map(|_| fresh_pixmap()).collect();
    let surface = SeparationSurface {
        pixmaps: &mut backend_plates,
        inks,
        base_transform: Transform::identity(),
    };
    let mut backend = SeparationBackend::new();
    backend.paint(&cmd, surface).unwrap();

    // Reference route: call `separation_renderer::fill_separation`
    // directly for each plate with the expected per-ink tint and
    // the same composed transform the backend would have used.
    let transform = combine_transforms(Transform::identity(), &ctm);
    let mut inline_plates: Vec<Pixmap> = (0..inks.len()).map(|_| fresh_pixmap()).collect();
    for (i, &tint) in tints.iter().enumerate() {
        crate::rendering::separation_renderer::fill_separation(
            &mut inline_plates[i],
            path,
            transform,
            tint,
            fill_rule,
            None,
        );
    }

    for (i, ink) in inks.iter().enumerate() {
        assert_eq!(
            backend_plates[i].data(),
            inline_plates[i].data(),
            "plate {:?} (index {i}) must match separation_renderer::fill_separation byte-for-byte",
            ink.as_str(),
        );
    }
}

#[test]
fn all_inks_paints_every_plate_at_same_tint() {
    // §8.6.6.3 Separation /All: every plate (process + spot) carries
    // the same tint. The override is carried on OverprintPlan; the
    // colour-resolution output is the alternate-space-evaluated
    // RGBA (composite-only), but the InkRouter consults the
    // selector and ignores the colour for routing.
    let path = rect_path();
    let cmd = ResolvedPaintCmd {
        kind: PaintKind::Path {
            path: &path,
            fill_rule: FillRule::Winding,
        },
        side: PaintSide::Fill,
        color: ResolvedColor::Rgba {
            r: 0.6,
            g: 0.6,
            b: 0.6,
            a: 1.0,
        },
        overprint: OverprintPlan {
            enabled: false,
            mode: 0,
            participating: SmallVec::new(),
            selector: InkSelector::All,
            all_tint: 0.6,
            spot_source: None,
            alt_cmyk_fallback: None,
        },
        blend: BlendPlan::Native(tiny_skia::BlendMode::SourceOver),
        clip: ClipPlan::None,
        ctm: Matrix::identity(),
    };
    let mut plates = vec![
        fresh_pixmap(),
        fresh_pixmap(),
        fresh_pixmap(),
        fresh_pixmap(),
    ];
    let inks = [
        InkName::new("Cyan"),
        InkName::new("Magenta"),
        InkName::new("PANTONE 185 C"),
        InkName::new("Dieline"),
    ];
    let surface = SeparationSurface {
        pixmaps: &mut plates,
        inks: &inks,
        base_transform: Transform::identity(),
    };
    let mut backend = SeparationBackend::new();
    backend.paint(&cmd, surface).unwrap();
    // 0.6 -> 153 (0.6 * 255 = 153.0).
    for (i, ink) in inks.iter().enumerate() {
        assert_eq!(
            plates[i].data()[(5 * 16 + 5) * 4],
            153,
            "/All must paint plate {:?} at the single tint",
            ink.as_str(),
        );
    }
}

#[test]
fn none_inks_paints_no_plates() {
    // §8.6.6.3 Separation /None: nothing visible. Every plate stays
    // at its initial zero value.
    let path = rect_path();
    let cmd = ResolvedPaintCmd {
        kind: PaintKind::Path {
            path: &path,
            fill_rule: FillRule::Winding,
        },
        side: PaintSide::Fill,
        color: ResolvedColor::Rgba {
            r: 0.0,
            g: 0.0,
            b: 0.0,
            a: 0.0,
        },
        overprint: OverprintPlan {
            enabled: false,
            mode: 0,
            participating: SmallVec::new(),
            selector: InkSelector::None,
            all_tint: 0.0,
            spot_source: None,
            alt_cmyk_fallback: None,
        },
        blend: BlendPlan::Native(tiny_skia::BlendMode::SourceOver),
        clip: ClipPlan::None,
        ctm: Matrix::identity(),
    };
    let mut plates = vec![fresh_pixmap(), fresh_pixmap()];
    let inks = [InkName::new("Cyan"), InkName::new("PANTONE 185 C")];
    let surface = SeparationSurface {
        pixmaps: &mut plates,
        inks: &inks,
        base_transform: Transform::identity(),
    };
    let mut backend = SeparationBackend::new();
    backend.paint(&cmd, surface).unwrap();
    // Both plates untouched.
    assert_eq!(plates[0].data()[(5 * 16 + 5) * 4], 0);
    assert_eq!(plates[1].data()[(5 * 16 + 5) * 4], 0);
}

#[test]
fn cmyk_cyan_only_matches_fill_separation_byte_for_byte() {
    // Single Cyan-only plate: backend paints Cyan at 0.5, knock-outs
    // other process plates at 0.0 (OP=false). Reference is
    // separation_renderer::fill_separation for each.
    let path = rect_path();
    let cmd = cmyk_cmd(&path, 0.5, 0.0, 0.0, 0.0);
    let inks = [
        InkName::new("Cyan"),
        InkName::new("Magenta"),
        InkName::new("Yellow"),
        InkName::new("Black"),
    ];
    let tints = [0.5, 0.0, 0.0, 0.0];
    assert_backend_matches_inline(
        &path,
        Matrix::identity(),
        cmd,
        &inks,
        &tints,
        FillRule::Winding,
    );
}

#[test]
fn cmyk_mixed_fill_matches_fill_separation_byte_for_byte() {
    // DeviceCMYK fill at (0.5, 0.25, 0.0, 0.7). Every process plate
    // must match its independent fill_separation invocation.
    let path = rect_path();
    let cmd = cmyk_cmd(&path, 0.5, 0.25, 0.0, 0.7);
    let inks = [
        InkName::new("Cyan"),
        InkName::new("Magenta"),
        InkName::new("Yellow"),
        InkName::new("Black"),
    ];
    let tints = [0.5, 0.25, 0.0, 0.7];
    assert_backend_matches_inline(
        &path,
        Matrix::identity(),
        cmd,
        &inks,
        &tints,
        FillRule::Winding,
    );
}

#[test]
fn cmyk_rotated_ctm_matches_fill_separation_byte_for_byte() {
    // Non-identity CTM: 30-degree rotation about origin, applied via
    // the command's `ctm` field. The backend composes ctm with
    // `base_transform`; the reference uses the same composition.
    // Mirrors the wave 5 inline-path rotated-rect probe.
    let path = rect_path();
    let theta = 30.0_f32.to_radians();
    let (s, c) = theta.sin_cos();
    let rotation = Matrix {
        a: c,
        b: s,
        c: -s,
        d: c,
        e: 0.0,
        f: 0.0,
    };
    let mut cmd = cmyk_cmd(&path, 0.5, 0.25, 0.0, 0.7);
    cmd.ctm = rotation;
    let inks = [
        InkName::new("Cyan"),
        InkName::new("Magenta"),
        InkName::new("Yellow"),
        InkName::new("Black"),
    ];
    let tints = [0.5, 0.25, 0.0, 0.7];
    assert_backend_matches_inline(&path, rotation, cmd, &inks, &tints, FillRule::Winding);
}
