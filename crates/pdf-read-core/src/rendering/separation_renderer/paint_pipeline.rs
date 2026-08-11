use super::*;

/// Dispatch a single paint operation through the resolution pipeline
/// and the [`SeparationBackend`]. Used for the spot / DeviceN / ICCBased
/// cases the inline `tint_for_ink` path can't resolve (notably Type-4
/// tint transforms on Separation/DeviceN sources). Returns `true` on a
/// successful pipeline dispatch; `false` if the colour can't be made
/// into a logical colour (caller falls back to the inline path).
#[allow(clippy::too_many_arguments)]
pub(super) fn paint_through_pipeline(
    fill: bool,
    fill_rule: Option<FillRule>,
    path: &tiny_skia::Path,
    pixmaps: &mut [Pixmap],
    target_inks: &[InkName],
    base_transform: Transform,
    gs: &GraphicsState,
    cs: &SeparationColorState,
    color_spaces: &HashMap<String, Object>,
    resources: &Object,
    doc: &PdfDocument,
    clip: Option<&Mask>,
    pipeline: &ResolutionPipeline,
    backend: &mut SeparationBackend,
) -> Result<()> {
    let _ = resources; // ResolutionContext consumes (doc, color_spaces); kept for future audits.
    let Some(logical) = logical_color_for_side(fill, gs, cs, color_spaces) else {
        return Ok(());
    };
    let side = if fill {
        PaintSide::Fill
    } else {
        PaintSide::Stroke
    };
    let intent = PaintIntent {
        kind: PaintKind::Path {
            path,
            fill_rule: fill_rule.unwrap_or(FillRule::Winding),
        },
        side,
        gs,
        color: logical,
        ctm: gs.ctm,
    };
    // Thread the same colour-policy borrows as the composite path
    // (page_renderer's run_pipeline_for_logical). The per-plate backend
    // consumes ResolvedColor::Cmyk channel-by-channel for plate routing
    // and never projects to RGBA, so the document /OutputIntents CMYK
    // profile carried here is effectively no-op for separations — the
    // plates ARE the press-target ink coverage. Threading it uniformly
    // keeps the resolver call surface symmetric with the composite path
    // so a single ColorResolver change can't silently diverge between
    // the two renderers.
    //
    // HONEST_GAP: the per-page `IccTransformCache` that amortises qcms
    // transform construction across paint operators lives on
    // `PageRenderer`. The separation walker is a free function — it
    // would need a SeparationRendererState struct to hold the cache
    // across paint operators within a page. That's a separate refactor;
    // the per-plate path doesn't actually invoke `cmyk_to_rgb_via_intent`
    // (the per-plate router consumes `ResolvedColor::Cmyk` directly),
    // so the only Transform construction here is on `/ICCBased` N=4
    // paint, and only when the embedded profile has a working CMM —
    // which is the design's expected (cold-path) case.
    let output_intent = doc.output_intent_cmyk_profile();
    let ctx = ResolutionContext::new(doc, color_spaces)
        .with_output_intent(output_intent.as_ref())
        .with_rendering_intent(crate::color::RenderingIntent::from_pdf_name(
            &gs.rendering_intent,
        ))
        .with_defaults(
            color_spaces.get("DefaultGray"),
            color_spaces.get("DefaultRGB"),
            color_spaces.get("DefaultCMYK"),
        );
    let cmd = pipeline.resolve(&intent, &ctx, None)?;
    // Wrap the clip mask back into a borrowed ClipPlan-equivalent via
    // the SeparationSurface's externally-visible state. The
    // SeparationBackend reads cmd.clip; build the cmd with an Arc-wrapped
    // mask only when one is present.
    let surface = SeparationSurface {
        pixmaps,
        inks: target_inks,
        base_transform,
    };
    // The pipeline currently produces ClipPlan::None because we passed
    // None into resolve(); for the separation walker the active clip
    // lives on `clip_stack` and is the same mask for every plate. Hand
    // it through by rebuilding the cmd with a wrapped Arc when present.
    let cmd = if let Some(mask) = clip {
        let mut new = cmd;
        new.clip = crate::rendering::resolution::ClipPlan::Mask(std::sync::Arc::new(mask.clone()));
        new
    } else {
        cmd
    };
    backend.paint(&cmd, surface)?;
    Ok(())
}

/// Decide whether the current paint at `gs.{fill,stroke}_color_space`
/// should route through the [`ResolutionPipeline`] or stay on the
/// inline `tint_for_ink` fast path.
///
/// The pipeline is the only path that handles Type-4 tint transforms,
/// Separation reserved colorant names (`/All`, `/None`), and the OPM=1
/// zero-component rule via [`InkRouter`]. Process colour direct
/// (`DeviceCMYK`, `DeviceGray`) and `DeviceRGB` (which the per-plate
/// path skips entirely) keep the existing inline behaviour — it's
/// cheaper and the inline arms are already correct for those cases.
pub(super) fn side_uses_pipeline(
    fill: bool,
    gs: &GraphicsState,
    color_spaces: &HashMap<String, Object>,
    resources: &Object,
    doc: &PdfDocument,
) -> bool {
    let space_name = if fill {
        &gs.fill_color_space
    } else {
        &gs.stroke_color_space
    };
    // Plain Device-* names take the inline path.
    if matches!(
        space_name.as_str(),
        "DeviceCMYK" | "CMYK" | "DeviceRGB" | "RGB" | "DeviceGray" | "G"
    ) {
        return false;
    }
    // Anything else: classify, and route compound spaces through the
    // pipeline so Type-4 / DeviceN / ICCBased N=4 evaluations land.
    matches!(
        resolve_color_space(space_name, color_spaces, resources, doc),
        ResolvedSpace::Separation(_)
            | ResolvedSpace::DeviceN(_)
            | ResolvedSpace::IccCmyk
            | ResolvedSpace::IccRgb
            | ResolvedSpace::IccGray
    )
}

/// Compute the initial colour components for a colour space per
/// ISO 32000-1 §8.6.4.2. `cs`/`CS` resets the current colour to these
/// values when entering the space.
pub(super) fn initial_components_for_space(
    space_name: &str,
    color_spaces: &HashMap<String, Object>,
    resources: &Object,
    doc: &PdfDocument,
) -> (Vec<f32>, Option<(f32, f32, f32, f32)>) {
    let resolved = resolve_color_space(space_name, color_spaces, resources, doc);
    match resolved {
        ResolvedSpace::Cmyk | ResolvedSpace::IccCmyk => {
            (vec![0.0, 0.0, 0.0, 1.0], Some((0.0, 0.0, 0.0, 1.0)))
        }
        ResolvedSpace::Rgb | ResolvedSpace::IccRgb => (vec![0.0, 0.0, 0.0], None),
        ResolvedSpace::Gray | ResolvedSpace::IccGray => (vec![0.0], None),
        ResolvedSpace::Separation(_) => (vec![1.0], None),
        ResolvedSpace::DeviceN(names) => {
            let n = names.len().max(1);
            (vec![1.0; n], None)
        }
        ResolvedSpace::Unknown => (Vec::new(), None),
    }
}
