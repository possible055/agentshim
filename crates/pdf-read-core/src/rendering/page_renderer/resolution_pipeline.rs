use super::*;

impl PageRenderer {
    /// Resolve the colours a path operator needs through the resolution
    /// pipeline and return a `GraphicsState` clone with the resolved RGBA
    /// spliced into the fields the rasteriser reads. Returns `None` when
    /// no side produced an RGBA the composite backend can consume
    /// directly — letting the caller borrow the original `gs` without
    /// allocating a clone.
    ///
    /// Path-fill (`f`/`F`/`f*`), path-stroke (`S`), and path
    /// fill-stroke combos (`B`/`b`/`B*`/`b*`) all flow through this;
    /// each variant of [`PipelinePaintKind`] decides which side(s) to
    /// resolve. Both sides resolve independently — the pipeline keys
    /// all of its side-specific behaviour off `intent.side`, so a Type 4
    /// Separation on the fill side and a plain DeviceRGB on the stroke
    /// side route correctly without contaminating each other.
    ///
    /// Text operators use the sibling
    /// [`Self::pipeline_resolve_text_colors`] — the text rasteriser
    /// already clones `gs` to advance `text_matrix`, so handing it
    /// colour overrides rather than a pre-cloned `GraphicsState` keeps
    /// the text path to one clone per operator instead of two.
    pub(crate) fn pipeline_resolve_paint_gs(
        &self,
        doc: &PdfDocument,
        gs: &GraphicsState,
        kind: PipelinePaintKind,
    ) -> Option<GraphicsState> {
        let (fills, strokes) = match kind {
            // ImageMask paints the stencil with the current fill colour
            // and never reads the stroke side; at this helper layer it
            // is semantically equivalent to PathFill. The variant is
            // kept distinct so the wave-5 separation-backend split can
            // dispatch on it without churning callers.
            PipelinePaintKind::PathFill | PipelinePaintKind::ImageMask => (true, false),
            PipelinePaintKind::PathStroke => (false, true),
            PipelinePaintKind::PathFillStroke => (true, true),
        };
        // Resolve, then short-circuit when the resolved RGBA already
        // equals the GS field that would supply it inline. For
        // Device-family inputs the resolver always returns Some but
        // the answer is the same colour the inline path would read,
        // so a clone here is wasted work. Skipping it keeps the
        // Device-family case allocation-free — the common path most
        // PDFs take.
        let fill_rgba = if fills {
            self.pipeline_resolve_rgba(doc, gs, PaintSide::Fill)
                .filter(|c| !rgba_matches(*c, gs.fill_color_rgb, gs.fill_alpha))
        } else {
            None
        };
        let stroke_rgba = if strokes {
            self.pipeline_resolve_rgba(doc, gs, PaintSide::Stroke)
                .filter(|c| !rgba_matches(*c, gs.stroke_color_rgb, gs.stroke_alpha))
        } else {
            None
        };
        if fill_rgba.is_none() && stroke_rgba.is_none() {
            return None;
        }
        let mut spliced = gs.clone();
        if let Some((r, g, b, a)) = fill_rgba {
            spliced.fill_color_rgb = (r, g, b);
            spliced.fill_alpha = a;
        }
        if let Some((r, g, b, a)) = stroke_rgba {
            spliced.stroke_color_rgb = (r, g, b);
            spliced.stroke_alpha = a;
        }
        Some(spliced)
    }

    /// Resolve the text-painting colours through the resolution
    /// pipeline and return them as side-tagged RGBA tuples for the text
    /// rasteriser to splice into its own `current_gs` clone. Returns
    /// `None` when the active `Tr` mode does not require any resolved
    /// side, or when neither side produced an RGBA the composite backend
    /// can consume directly — letting the caller hand the rasteriser
    /// the unmodified `gs` reference.
    ///
    /// Mirrors the side-selection logic of
    /// [`Self::pipeline_resolve_paint_gs`] but returns colours rather
    /// than a `GraphicsState` clone: the text rasteriser already clones
    /// `gs` to walk `text_matrix` per glyph (or per `TJ` element), so
    /// it splices the overrides into that clone — eliminating the
    /// operator-arm-side clone we would otherwise pay on every `Tj` /
    /// `TJ` / `'` / `"`.
    ///
    /// `Tr`-mode handling (ISO 32000-1 §9.3.6 Table 106):
    /// * `0`, `2`, `4`, `6` fill the glyph → resolve fill side.
    /// * `1`, `2`, `5`, `6` stroke the glyph → resolve stroke side.
    /// * `3` is invisible (no painting); skip resolution entirely so
    ///   PDFs that emit text-as-OCR-overlay don't pay any pipeline
    ///   cost.
    pub(crate) fn pipeline_resolve_text_colors(
        &self,
        doc: &PdfDocument,
        gs: &GraphicsState,
    ) -> Option<ResolvedColors> {
        if gs.render_mode == 3 {
            return None;
        }
        // Same short-circuit as the path helper: a resolved RGBA that
        // matches the GS field the rasteriser would read inline is a
        // no-op override. Filtering it out lets the operator arm pass
        // `None` straight through and skip the per-element
        // `paint.set_color` write inside `render_text`.
        let fill = if matches!(gs.render_mode, 0 | 2 | 4 | 6) {
            self.pipeline_resolve_rgba(doc, gs, PaintSide::Fill)
                .filter(|c| !rgba_matches(*c, gs.fill_color_rgb, gs.fill_alpha))
        } else {
            None
        };
        let stroke = if matches!(gs.render_mode, 1 | 2 | 5 | 6) {
            self.pipeline_resolve_rgba(doc, gs, PaintSide::Stroke)
                .filter(|c| !rgba_matches(*c, gs.stroke_color_rgb, gs.stroke_alpha))
        } else {
            None
        };
        let colors = ResolvedColors { fill, stroke };
        if colors.is_empty() {
            None
        } else {
            Some(colors)
        }
    }

    /// Resolve the active colour for `side` through the resolution pipeline.
    /// Returns `None` when the resolver produces a non-RGBA variant the
    /// composite backend cannot consume directly (per-channel outputs
    /// reserved for separation backends).
    ///
    /// Routes the current colour through [`ResolutionPipeline`], which
    /// handles `Separation`/`DeviceN` colour spaces backed by PostScript
    /// Type 4 tint transforms — the case the inline match arms used to
    /// evaluate as `1.0 - tint` before wave 5 deleted the fallback.
    ///
    /// Fill and stroke share one helper because the only differences are
    /// which `gs` fields supply the colour and which `PaintSide` the
    /// pipeline routes against. The pipeline's colour stage already
    /// keys all of its side-specific behaviour (e.g. alpha fold) off
    /// `intent.side`.
    pub(super) fn pipeline_resolve_rgba(
        &self,
        doc: &PdfDocument,
        gs: &GraphicsState,
        side: PaintSide,
    ) -> Option<(f32, f32, f32, f32)> {
        let (space_name, components) = match side {
            PaintSide::Fill => (gs.fill_color_space.as_str(), &gs.fill_color_components),
            PaintSide::Stroke => (gs.stroke_color_space.as_str(), &gs.stroke_color_components),
        };
        let resolved_space_obj = self.color_spaces.get(space_name);
        let logical = build_logical_color(space_name, components, resolved_space_obj);
        self.run_pipeline_for_logical(doc, &self.color_spaces, logical, gs, side)
    }

    /// `gs`-free overload of the colour-resolution path: route an
    /// explicit colour-space + components tuple through the pipeline and
    /// return the resolved RGBA.
    ///
    /// The path/text/image-mask helpers above read their colour inputs
    /// from `gs.fill_color_space` / `gs.fill_color_components` (or the
    /// stroke equivalents). Shading endpoint colours don't live there —
    /// they sit in the shading dictionary's `/Function /C0` and `/C1`
    /// arrays, alongside the shading dictionary's own `/ColorSpace`. The
    /// dispatcher needs to resolve those two endpoints independently
    /// of `gs` so the gradient backend can hand them to the
    /// interpolator as fixed stops. This helper is that hook: caller
    /// supplies the shading's `/ColorSpace` object directly and the
    /// per-endpoint component list; the helper builds the logical
    /// colour, runs it through the pipeline against a synthesised
    /// graphics state carrying only the requested alpha (every other
    /// `gs` field — blend mode, overprint — is irrelevant for endpoint
    /// resolution because the gradient is composited as a single Source
    /// Over fill by the caller), and returns the RGBA.
    ///
    /// Returns `None` only when the resolver produces a non-RGBA variant
    /// (per-channel outputs reserved for separation backends). The
    /// caller is then expected to fall back to its inline behaviour.
    pub(crate) fn pipeline_resolve_components(
        &self,
        doc: &PdfDocument,
        color_spaces: &HashMap<String, Object>,
        space: &Object,
        components: &[f32],
        alpha: f32,
    ) -> Option<(f32, f32, f32, f32)> {
        // Two shapes appear in real PDFs for a shading dict's
        // `/ColorSpace`: a Name (either a Device alias like
        // `/DeviceRGB` or a per-page resource name like `/CS1`), or an
        // inline Array (e.g. `[/Separation /MagentaSpot /DeviceCMYK
        // funcRef]`). `build_logical_color` already handles both via
        // its name + `Option<&Object>` arguments, so this wrapper just
        // dispatches into it; inline arrays get the empty name so the
        // Device-family fast-path doesn't fire.
        let (space_name, resolved_space): (&str, Option<&Object>) = match space {
            Object::Name(n) => (n.as_str(), color_spaces.get(n.as_str())),
            other => ("", Some(other)),
        };
        let logical = build_logical_color(space_name, components, resolved_space);

        // The pipeline reads `gs.fill_alpha` for fill-side alpha fold.
        // A synthesised default `GraphicsState` patched with `alpha`
        // produces the correct RGBA; overprint / blend plans on the
        // synth gs are produced but discarded — only the colour is
        // returned.
        let mut synth_gs = GraphicsState::new();
        synth_gs.fill_alpha = alpha;
        self.run_pipeline_for_logical(doc, color_spaces, logical, &synth_gs, PaintSide::Fill)
    }

    /// Core resolver step shared between [`Self::pipeline_resolve_rgba`]
    /// (gs-bound path-side resolution) and
    /// [`Self::pipeline_resolve_components`] (gs-free shading-endpoint
    /// resolution). Builds the [`PaintIntent`], runs the pipeline, and
    /// projects the resolved colour down to an RGBA tuple — returning
    /// `None` for non-RGBA variants the composite backend cannot
    /// consume directly.
    pub(super) fn run_pipeline_for_logical(
        &self,
        doc: &PdfDocument,
        color_spaces: &HashMap<String, Object>,
        logical: LogicalColor<'_>,
        gs: &GraphicsState,
        side: PaintSide,
    ) -> Option<(f32, f32, f32, f32)> {
        let pipeline = ResolutionPipeline::new();
        // Document /OutputIntents CMYK profile + page-level
        // /Default[Gray|RGB|CMYK] (§8.6.5.6) + graphics-state rendering
        // intent (§10.7.3) feed the colour stage's ICC dispatch. The
        // `output_intent_cmyk_profile()` accessor already filters for
        // /N=4 and parses the embedded stream; we just hand the Arc
        // (when present) to the context.
        let output_intent = doc.output_intent_cmyk_profile();
        // Hand the per-page CMYK transform cache to the resolver. The
        // cache lives on `Self` (cleared at render start in
        // `render_page_with_options`); threading it here is what
        // turns the 1000-paint same-colour case from "rebuild qcms
        // transform 1000×" into "cache miss once, hit 999×".
        let ctx = ResolutionContext::new(doc, color_spaces)
            .with_output_intent(output_intent.as_ref())
            .with_rendering_intent(crate::color::RenderingIntent::from_pdf_name(
                &gs.rendering_intent,
            ))
            .with_defaults(
                color_spaces.get("DefaultGray"),
                color_spaces.get("DefaultRGB"),
                color_spaces.get("DefaultCMYK"),
            )
            .with_icc_transform_cache(Some(&self.icc_transform_cache));
        // No geometry is needed: the colour stage only reads `color`
        // (and reads `gs` for the alpha fold). `ColorOnly` lets the
        // intent express that without conjuring a placeholder path.
        let intent = PaintIntent {
            kind: PaintKind::ColorOnly,
            side,
            gs,
            color: logical,
            ctm: gs.ctm,
        };
        let cmd = pipeline.resolve(&intent, &ctx, None).ok()?;
        match cmd.color {
            ResolvedColor::Rgba { r, g, b, a } => Some((r, g, b, a)),
            // Genuine DeviceCMYK sources, plus Separation and DeviceN
            // with a DeviceCMYK alternate, emit `Cmyk` so the per-plate
            // backend has the channel decomposition. Project to RGBA
            // via the context-aware CMYK→RGB path: consult the
            // document's /OutputIntents CMYK profile when present, fall
            // back to the process-ink conversion otherwise.
            ResolvedColor::Cmyk { c, m, y, k, a } => {
                let (r, g, b) =
                    crate::rendering::resolution::color::cmyk_to_rgb_via_intent(c, m, y, k, &ctx);
                Some((r, g, b, a))
            }
            // /ICCBased N=4 with a parseable embedded profile that
            // compiled a usable CMM. Per §8.6.5.5 the embedded profile
            // is THE conversion source for this colour space — it
            // overrides the document /OutputIntents — so the RGB on
            // this variant is already the right composite output. The
            // CMYK side-payload is for the per-plate router only.
            ResolvedColor::IccCmyk { r, g, b, a, .. } => Some((r, g, b, a)),
            _ => None,
        }
    }
}
