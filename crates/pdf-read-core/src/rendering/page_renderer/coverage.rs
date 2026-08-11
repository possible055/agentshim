use super::*;

impl PageRenderer {
    /// Recompute every painted pixel through the §11.4 compose-first
    /// rule. The naive paint path converted CMYK→RGB through the
    /// OutputIntent ICC before alpha-blending; under a non-linear ICC
    /// (input curves != identity), `ICC(α·A + (1-α)·B) ≠ α·ICC(A) +
    /// (1-α)·ICC(B)`, so the convert-first result diverges from the
    /// spec-correct compose-first value. This helper recovers the
    /// effective coverage from the post-paint RGB (using the convert-
    /// first source RGB the rasteriser actually wrote) and replaces the
    /// pixel with `ICC(α·source_cmyk + (1-α)·snapshot_cmyk)`, where
    /// `snapshot_cmyk` comes from inverting the snapshot RGB through
    /// the additive-clamp formula. The inversion is exact when the
    /// snapshot was produced by an additive-clamp paint (the
    /// no-transparency baseline) and is the same lossy approximation
    /// the composite overprint path admits when the backdrop went
    /// through a non-trivial ICC.
    ///
    /// Alpha channel is preserved from the post-paint pixmap because
    /// the alpha composition rule is the same in either ordering
    /// (`α_out = c·α_src + (1-c·α_src)·α_dst`).
    /// Rasterise a fill path to a coverage byte buffer when the CMYK
    /// sidecar is active. Returns `None` when the sidecar is
    /// detection-OFF — the diff-driven compose-first path is the
    /// only one used in that case and a coverage mask would be
    /// unused work.
    pub(super) fn rasterise_fill_coverage(
        &self,
        path: &tiny_skia::Path,
        transform: Transform,
        fill_rule: tiny_skia::FillRule,
        clip: Option<&tiny_skia::Mask>,
    ) -> Option<Vec<u8>> {
        let sidecar = self.cmyk_sidecar.as_ref()?;
        let (w, h) = sidecar.dims();
        let mut mask = tiny_skia::Mask::new(w, h)?;
        mask.fill_path(path, fill_rule, true, transform);
        let mut buf = mask.data().to_vec();
        // Intersect with the active clip mask. tiny_skia's clip mask
        // is per-pixel coverage; pixel-wise min gives the
        // intersection.
        if let Some(c) = clip {
            for (b, cv) in buf.iter_mut().zip(c.data().iter()) {
                *b = (*b).min(*cv);
            }
        }
        Some(buf)
    }

    /// Rasterise a stroke path to a coverage byte buffer. Mirror of
    /// [`Self::rasterise_fill_coverage`] for the stroke-side compose-
    /// first / overprint paths. tiny_skia's `Mask` does not expose
    /// `stroke_path` directly, so this routes through a scratch
    /// alpha-only `Pixmap`: paint the stroke with full-alpha black,
    /// then extract the alpha channel as the coverage buffer.
    pub(super) fn rasterise_stroke_coverage(
        &self,
        path: &tiny_skia::Path,
        transform: Transform,
        gs: &GraphicsState,
        clip: Option<&tiny_skia::Mask>,
    ) -> Option<Vec<u8>> {
        let sidecar = self.cmyk_sidecar.as_ref()?;
        let (w, h) = sidecar.dims();
        let mut scratch = Pixmap::new(w, h)?;
        let dash = if !gs.dash_pattern.0.is_empty() {
            tiny_skia::StrokeDash::new(gs.dash_pattern.0.clone(), gs.dash_pattern.1)
        } else {
            None
        };
        let stroke = tiny_skia::Stroke {
            width: gs.line_width,
            line_cap: match gs.line_cap {
                1 => tiny_skia::LineCap::Round,
                2 => tiny_skia::LineCap::Square,
                _ => tiny_skia::LineCap::Butt,
            },
            line_join: match gs.line_join {
                1 => tiny_skia::LineJoin::Round,
                2 => tiny_skia::LineJoin::Bevel,
                _ => tiny_skia::LineJoin::Miter,
            },
            miter_limit: gs.miter_limit,
            dash,
        };
        let mut paint = tiny_skia::Paint::default();
        paint.set_color(tiny_skia::Color::from_rgba8(0, 0, 0, 255));
        paint.anti_alias = true;
        scratch.stroke_path(path, &paint, &stroke, transform, clip);
        let buf: Vec<u8> = scratch.data().chunks_exact(4).map(|px| px[3]).collect();
        Some(buf)
    }

    /// Build a coverage-only `GraphicsState` clone from `gs`. The clone
    /// forces full opacity (`fill_alpha` / `stroke_alpha` = 1.0),
    /// `/Normal` blend, and opaque-black fill colour. Re-running a paint
    /// with this gs into a fresh transparent scratch pixmap produces an
    /// alpha channel that equals geometry coverage at every pixel — the
    /// same per-pixel coverage `tiny_skia::Mask::fill_path` and the
    /// stroke-side scratch-Pixmap helper produce for path-side coverage.
    /// The caller extracts the alpha channel via
    /// [`Self::extract_alpha_as_coverage`].
    ///
    /// `gs.render_mode` is preserved verbatim. ISO 32000-1 §9.3.6 text
    /// rendering mode 3 ("neither fill nor stroke; add to path for
    /// clipping") produces no visible mark, and under the §11.3.3
    /// single shape/opacity per pixel rule the spot lane must see no
    /// mark either (§11.7.3 composes the spot lane with the same shape
    /// / opacity as the page). The text rasteriser already collapses
    /// the paint to fully transparent for `render_mode == 3` (see
    /// `text_rasterizer.rs` — `paint.set_color(rgba 0,0,0,0)`), so the
    /// scratch alpha channel correctly resolves to zero coverage and no
    /// spot lane write fires. Overriding `render_mode` to 0 here would
    /// paint visible glyphs into the coverage scratch while the visible
    /// pixmap shows nothing, leaking a spurious spot-lane write.
    pub(super) fn coverage_only_gs(gs: &GraphicsState) -> GraphicsState {
        let mut cov = gs.clone();
        cov.fill_alpha = 1.0;
        cov.stroke_alpha = 1.0;
        cov.blend_mode = "Normal".to_string();
        cov.fill_color_rgb = (0.0, 0.0, 0.0);
        cov.stroke_color_rgb = (0.0, 0.0, 0.0);
        // Strip SMask so the scratch render doesn't kick off a
        // recursive SMask compose with a different geometry.
        cov.smask = None;
        // Force a fill-producing render mode. The visible mode may be 7
        // (clip-only) or 3 (invisible), both of which the text rasteriser
        // deliberately paints with transparent paint (WS1.5) — routing the
        // coverage render through those modes would yield an empty silhouette
        // and silently drop the clip. Mode 0 fills the glyph body opaquely,
        // which is exactly the coverage the clip accumulation needs.
        cov.render_mode = 0;
        cov
    }

    /// Extract the alpha channel from a pixmap as a byte buffer. The
    /// alpha encodes per-pixel coverage when the pixmap was painted
    /// with opaque-black paint and `BlendMode::SourceOver` on a fresh
    /// transparent backdrop — both glyph fills, image blits, and
    /// shading paints obey that contract through the existing
    /// rasterisers when the gs has `fill_alpha = 1.0` and
    /// `blend_mode = "Normal"`. Per pixel: `alpha == 255` is fully
    /// covered, `alpha == 0` is uncovered, intermediate values carry
    /// AA-edge partial coverage. The buffer is then handed to the
    /// spot-mirror's coverage-aware path verbatim.
    pub(super) fn extract_alpha_as_coverage(pixmap: &Pixmap) -> Vec<u8> {
        pixmap.data().chunks_exact(4).map(|px| px[3]).collect()
    }

    /// WS1.5b — union a clip-mode (`Tr` 4–7) `Tj` / `'` / `"` show's glyph
    /// outlines into the text-clip accumulator.
    ///
    /// `accum` is a page-sized scratch pixmap whose alpha channel holds the
    /// accumulated glyph silhouette for the enclosing `BT`…`ET` block; it is
    /// created lazily on the first clip-mode show so modes 0–3 never allocate
    /// it. Glyphs are laid down with [`Self::coverage_only_gs`] (opaque black,
    /// `SourceOver`), so each show's outlines union with the previous ones in
    /// place — exactly the "add to the current clip path" semantics of ISO
    /// 32000-1 §9.4.1. [`Self::coverage_only_gs`] forces fill mode 0 so the
    /// glyph bodies rasterise opaquely even when the visible mode is 7
    /// (clip-only) or 3 (invisible), which the rasteriser paints transparent.
    /// The inherited clip is intentionally *not* applied here; the final `ET`
    /// intersection folds the silhouette into the live clip.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn accumulate_text_clip_tj(
        &self,
        accum: &mut Option<Pixmap>,
        width: u32,
        height: u32,
        text: &[u8],
        transform: Transform,
        gs: &GraphicsState,
        resources: &Object,
        doc: &PdfDocument,
    ) {
        if accum.is_none() {
            *accum = Pixmap::new(width, height);
        }
        let Some(scratch) = accum.as_mut() else {
            return;
        };
        let cov_gs = Self::coverage_only_gs(gs);
        // Coverage raster is permitted to fail silently — the visible-paint
        // call for the same show already surfaces any real error, and a
        // missing silhouette simply means no clip contribution.
        let _ = self.text_rasterizer.render_text(
            scratch,
            text,
            transform,
            &cov_gs,
            None,
            resources,
            doc,
            None,
            &self.fonts,
        );
    }

    /// WS1.5b — `TJ` positioning-array counterpart of
    /// [`Self::accumulate_text_clip_tj`]. Same contract.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn accumulate_text_clip_tj_array(
        &self,
        accum: &mut Option<Pixmap>,
        width: u32,
        height: u32,
        array: &[crate::content::operators::TextElement],
        transform: Transform,
        gs: &GraphicsState,
        resources: &Object,
        doc: &PdfDocument,
    ) {
        if accum.is_none() {
            *accum = Pixmap::new(width, height);
        }
        let Some(scratch) = accum.as_mut() else {
            return;
        };
        let cov_gs = Self::coverage_only_gs(gs);
        let _ = self.text_rasterizer.render_tj_array(
            scratch,
            array,
            transform,
            &cov_gs,
            None,
            resources,
            doc,
            None,
            &self.fonts,
        );
    }

    /// Rasterise the text-show coverage for a single `Tj` / `'` / `"`
    /// string by running the same `text_rasterizer.render_text` path
    /// the visible paint uses, but with [`Self::coverage_only_gs`] so
    /// the alpha channel encodes per-glyph AA-edge coverage exactly.
    /// Returns `None` when the sidecar is detection-OFF (coverage
    /// would be unused work).
    ///
    /// Per ISO 32000-1 §9.4 text-showing operators + §9.6 simple-font
    /// glyph rasterisation: every glyph in the run is laid into the
    /// scratch pixmap via the same tt-parser / harfrust / ttf-outline
    /// path the visible paint uses, so the coverage mask is geometry-
    /// identical (including font-fallback substitutions) to the
    /// visible glyph bodies.
    pub(super) fn rasterise_text_coverage_render_text(
        &self,
        text: &[u8],
        base_transform: Transform,
        gs: &GraphicsState,
        resources: &Object,
        doc: &PdfDocument,
        clip_mask: Option<&tiny_skia::Mask>,
    ) -> Option<Vec<u8>> {
        let sidecar = self.cmyk_sidecar.as_ref()?;
        let (w, h) = sidecar.dims();
        let mut scratch = Pixmap::new(w, h)?;
        let cov_gs = Self::coverage_only_gs(gs);
        // Suppress error logs — the coverage scratch path is permitted
        // to fail silently because the visible-paint call will have
        // already surfaced the same error.
        let _ = self.text_rasterizer.render_text(
            &mut scratch,
            text,
            base_transform,
            &cov_gs,
            None,
            resources,
            doc,
            clip_mask,
            &self.fonts,
        );
        Some(Self::extract_alpha_as_coverage(&scratch))
    }

    /// Rasterise the text-show coverage for a `TJ` array. Mirror of
    /// [`Self::rasterise_text_coverage_render_text`] for the
    /// positioning-adjustment form. Same §9.4 + §9.6 contract.
    pub(super) fn rasterise_text_coverage_render_tj_array(
        &self,
        array: &[crate::content::operators::TextElement],
        base_transform: Transform,
        gs: &GraphicsState,
        resources: &Object,
        doc: &PdfDocument,
        clip_mask: Option<&tiny_skia::Mask>,
    ) -> Option<Vec<u8>> {
        let sidecar = self.cmyk_sidecar.as_ref()?;
        let (w, h) = sidecar.dims();
        let mut scratch = Pixmap::new(w, h)?;
        let cov_gs = Self::coverage_only_gs(gs);
        let _ = self.text_rasterizer.render_tj_array(
            &mut scratch,
            array,
            base_transform,
            &cov_gs,
            None,
            resources,
            doc,
            clip_mask,
            &self.fonts,
        );
        Some(Self::extract_alpha_as_coverage(&scratch))
    }

    /// Rasterise the coverage for an Image / ImageMask Do by re-running
    /// the same image / stencil paint path into a fresh transparent
    /// scratch pixmap with [`Self::coverage_only_gs`] (fill_alpha = 1,
    /// /Normal BM). The resulting alpha channel folds the unit-square
    /// device-space footprint (§8.9.5) with the per-pixel stencil bit
    /// (§8.9.6.2 /Decode default) for ImageMasks AND with the per-
    /// pixel alpha of the source image for sampled images.
    ///
    /// Returns `None` when the sidecar is detection-OFF or when the
    /// XObject is a Form (Form Do is handled by the per-paint mirror
    /// inside the form's recursive content stream — the post-Do mirror
    /// for Form XObjects is suppressed by round 3's P0 fix).
    pub(super) fn rasterise_image_xobject_coverage(
        &mut self,
        name: &str,
        transform: Transform,
        gs: &GraphicsState,
        resources: &Object,
        doc: &PdfDocument,
        clip_mask: Option<&tiny_skia::Mask>,
    ) -> Option<Vec<u8>> {
        let sidecar = self.cmyk_sidecar.as_ref()?;
        let (w, h) = sidecar.dims();
        let mut scratch = Pixmap::new(w, h)?;
        let cov_gs = Self::coverage_only_gs(gs);
        // Resolve the XObject reference + subtype dispatch the same
        // way the visible-paint Do arm does, but only for Image and
        // ImageMask subtypes. Form XObjects are excluded because
        // their post-Do mirror is suppressed (round 3 P0 fix), and
        // because re-running a Form Do here would invoke its own
        // nested content stream recursively — work that has nothing
        // to do with coverage extraction on the OUTER Do site.
        let xobj_dict_resources = resources;
        if let Object::Dictionary(res_dict) = xobj_dict_resources {
            if let Some(xobj_entry) = res_dict.get("XObject") {
                let xobjects_obj = doc.resolve_object(xobj_entry).ok()?;
                if let Some(xobjects) = xobjects_obj.as_dict() {
                    if let Some(xobj_ref_obj) = xobjects.get(name) {
                        let xobj = doc.resolve_object(xobj_ref_obj).ok()?;
                        let xobj_ref = xobj_ref_obj.as_reference();
                        if let Object::Stream { ref dict, .. } = xobj {
                            if let Some(subtype) = dict.get("Subtype").and_then(|o| o.as_name()) {
                                if subtype == "Image" {
                                    let is_image_mask = dict
                                        .get("ImageMask")
                                        .map(|o| matches!(o, Object::Boolean(true)))
                                        .unwrap_or(false);
                                    if is_image_mask {
                                        let _ = self.render_image_mask(
                                            &mut scratch,
                                            &xobj,
                                            xobj_ref,
                                            transform,
                                            doc,
                                            clip_mask,
                                            &cov_gs,
                                        );
                                    } else {
                                        let smask = dict.get("SMask").cloned();
                                        let mask = dict.get("Mask").cloned();
                                        let _ = self.render_image(
                                            &mut scratch,
                                            &xobj,
                                            xobj_ref,
                                            transform,
                                            doc,
                                            clip_mask,
                                            smask,
                                            mask,
                                            &cov_gs,
                                        );
                                    }
                                } else {
                                    // Form XObject (or other): no
                                    // coverage from this site —
                                    // returning all-zero coverage
                                    // would over-suppress the spot
                                    // mirror's diff fallback. Instead
                                    // signal "no coverage produced"
                                    // by returning None; the spot
                                    // mirror falls back to the diff
                                    // branch.
                                    return None;
                                }
                            }
                        }
                    }
                }
            }
        }
        Some(Self::extract_alpha_as_coverage(&scratch))
    }

    /// Resolve the shading dict's spot-ink list. Returns
    /// `Some(non_empty)` when the shading's `/ColorSpace` is
    /// `/Separation` or a non-process `/DeviceN`, with the tints taken
    /// from the function's `/C0` endpoint (correct for constant
    /// gradients; for varying gradients the C0 tint is the LANE write
    /// the §11.3.3 compose will see — a single tint per ink is the
    /// most the current spot-mirror representation supports).
    ///
    /// Returns `None` when the shading isn't found, has no
    /// `/ColorSpace`, or its CS is a process colour space.
    pub(super) fn resolve_shading_spot_inks(
        &self,
        name: &str,
        resources: &Object,
        doc: &PdfDocument,
    ) -> Option<Vec<(String, f32)>> {
        // Walk Resources/Shading/<name> the same way render_shading
        // does.
        let res_dict = resources.as_dict()?;
        let shadings_obj = res_dict.get("Shading")?;
        let shadings = doc.resolve_object(shadings_obj).ok()?;
        let shadings_dict = shadings.as_dict()?;
        let sh_obj = shadings_dict.get(name)?;
        let shading = doc.resolve_object(sh_obj).ok()?;
        let shading_dict = shading.as_dict()?;

        // Get /ColorSpace (Name | Array).
        let cs_obj = shading_dict.get("ColorSpace")?;
        let cs_resolved = doc.resolve_object(cs_obj).ok()?;

        // The CS might be a Name pointing into the page Resources
        // ColorSpace dict. Walk it to its array form so
        // `extract_paint_spot_inks` can match against the
        // `/Separation` / `/DeviceN` head.
        let cs_array_object: Object = if let Some(cs_name) = cs_resolved.as_name() {
            let cs_dict_obj = res_dict.get("ColorSpace")?;
            let cs_dict_resolved = doc.resolve_object(cs_dict_obj).ok()?;
            let cs_dict = cs_dict_resolved.as_dict()?;
            let named = cs_dict.get(cs_name)?;
            doc.resolve_object(named).ok()?
        } else {
            cs_resolved
        };

        // Extract the function's /C0 endpoint (used for constant
        // gradients; for Type 2 functions this is the value at
        // /Domain[0]).
        let func_obj = shading_dict.get("Function")?;
        let func_resolved = doc.resolve_object(func_obj).ok()?;
        let func_dict = func_resolved.as_dict()?;
        let c0_obj = func_dict.get("C0")?;
        let c0_arr = c0_obj.as_array()?;
        let c0_components: Vec<f32> = c0_arr
            .iter()
            .map(|o| match o {
                Object::Real(v) => *v as f32,
                Object::Integer(v) => *v as f32,
                _ => 0.0,
            })
            .collect();

        // Dispatch through the existing spot-extractor.
        let inks = crate::rendering::sidecar::extract_paint_spot_inks(
            &cs_array_object,
            &c0_components,
            doc,
        );
        if inks.is_empty() {
            None
        } else {
            Some(inks)
        }
    }

    /// Rasterise the coverage for a shading paint (`sh` operator) by
    /// re-running `render_shading` into a fresh transparent scratch
    /// pixmap with [`Self::coverage_only_gs`] (fill_alpha = 1, /Normal
    /// BM). The shading interpolator paints its gradient colour into
    /// the scratch, and the alpha channel records per-pixel coverage
    /// of the gradient geometry intersected with the active clip
    /// (§8.7.4).
    ///
    /// Returns `None` when the sidecar is detection-OFF.
    pub(super) fn rasterise_shading_coverage(
        &self,
        name: &str,
        transform: Transform,
        gs: &GraphicsState,
        resources: &Object,
        doc: &PdfDocument,
        clip_mask: Option<&tiny_skia::Mask>,
    ) -> Option<Vec<u8>> {
        let sidecar = self.cmyk_sidecar.as_ref()?;
        let (w, h) = sidecar.dims();
        let mut scratch = Pixmap::new(w, h)?;
        let cov_gs = Self::coverage_only_gs(gs);
        let _ = self.render_shading(
            &mut scratch,
            name,
            transform,
            &cov_gs,
            resources,
            doc,
            clip_mask,
        );
        Some(Self::extract_alpha_as_coverage(&scratch))
    }
}
