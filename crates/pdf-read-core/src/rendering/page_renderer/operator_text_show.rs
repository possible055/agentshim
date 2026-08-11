use super::*;

impl PageRenderer {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn execute_text_show_operator(
        &mut self,
        op: &Operator,
        pixmap: &mut Pixmap,
        base_transform: Transform,
        gs_stack: &mut GraphicsStateStack,
        clip_stack: &[Option<tiny_skia::Mask>],
        text_clip_accum: &mut Option<Pixmap>,
        in_text_object: bool,
        excluded_layer_depth: u32,
        doc: &PdfDocument,
        page_num: usize,
        resources: &Object,
    ) -> Result<bool> {
        match op {
            Operator::Tj { text } => {
                if in_text_object {
                    // Type 3 fonts have no outline program; each glyph is a
                    // CharProcs content stream painted under FontMatrix ×
                    // text-space × CTM (ISO 32000-1 §9.6.5). It is handled
                    // here because it re-enters the content-stream renderer.
                    if self.current_font_is_type3(gs_stack.current()) {
                        let advance = if excluded_layer_depth == 0 {
                            let gs_snap = gs_stack.current().clone();
                            self.render_type3_text(
                                pixmap,
                                text,
                                base_transform,
                                &gs_snap,
                                doc,
                                page_num,
                                resources,
                            )?
                        } else {
                            self.text_rasterizer
                                .measure_text(text, gs_stack.current(), &self.fonts)
                        };
                        gs_stack.current_mut().advance_text_matrix(advance);
                        return Ok(true);
                    }
                    let gs = gs_stack.current();
                    let advance = if excluded_layer_depth == 0 {
                        let clip = clip_stack.last().and_then(|c| c.as_ref());
                        let transform = combine_transforms(base_transform, &gs.ctm);
                        // WS1.5b — modes 4–7 add this show's glyph
                        // outlines to the text clip path (applied at ET).
                        // Gated here so modes 0–3 pay nothing.
                        if gs.render_mode >= 4 {
                            self.accumulate_text_clip_tj(
                                text_clip_accum,
                                pixmap.width(),
                                pixmap.height(),
                                text,
                                transform,
                                gs,
                                resources,
                                doc,
                            );
                        }
                        // Resolve the fill (and/or stroke per Tr mode)
                        // once for the whole `Tj` call and hand the
                        // resolved RGBA to the rasteriser. The rasteriser
                        // already clones `gs` to advance `text_matrix`
                        // per element, so it splices the override into
                        // that clone — no operator-arm-side clone
                        // needed.
                        let colors = self.pipeline_resolve_text_colors(doc, gs);
                        // §11.4.7 + §11.7.4 + §11.4 cycle: text-
                        // showing is a fill-side paint (modulated by
                        // Tr render mode for stroke). One snapshot
                        // per Tj call brackets the whole string.
                        let smask_snap = self.smask_snapshot(pixmap, gs);
                        let smask_spot_snap = self.smask_spot_snapshot(gs);
                        let overprint_snap = self.overprint_snapshot(pixmap, gs, true);
                        let cmyk_compose_snap = self.cmyk_compose_snapshot(pixmap, gs, doc, true);
                        let spot_snap = self.text_fill_spot_snapshot(pixmap, gs);
                        // §9.4 + §11.7.3 + §11.3.3: rasterise the
                        // glyph-outline coverage in parallel with
                        // the visible paint so the spot mirror has
                        // a geometry-true per-pixel coverage mask
                        // (AA-edge fidelity + identical-RGB
                        // collision insulated) instead of a
                        // snapshot-vs-post-paint diff.
                        let text_coverage = spot_snap.as_ref().and_then(|_| {
                            self.rasterise_text_coverage_render_text(
                                text, transform, gs, resources, doc, clip,
                            )
                        });
                        let adv = self.text_rasterizer.render_text(
                            pixmap,
                            text,
                            transform,
                            gs,
                            colors.as_ref(),
                            resources,
                            doc,
                            clip,
                            &self.fonts,
                        )?;
                        let gs_for_apply = gs_stack.current().clone();
                        if let Some(snap) = cmyk_compose_snap {
                            self.apply_cmyk_compose_after_paint(
                                pixmap,
                                &snap,
                                &gs_for_apply,
                                doc,
                                true,
                            );
                        }
                        if let Some(snap) = overprint_snap {
                            self.apply_overprint_after_paint(
                                pixmap,
                                &snap,
                                &gs_for_apply,
                                doc,
                                true,
                            );
                        }
                        if let Some(snap) = spot_snap {
                            self.mirror_spot_paint_into_sidecar_with_coverage(
                                pixmap,
                                &snap,
                                text_coverage.as_deref(),
                                &gs_for_apply,
                                true,
                            );
                        }
                        if let Some(snap) = smask_snap {
                            self.apply_smask_after_paint(
                                pixmap,
                                &snap,
                                smask_spot_snap.as_deref(),
                                &gs_for_apply,
                                doc,
                                page_num,
                                resources,
                                base_transform,
                            )?;
                        }
                        adv
                    } else {
                        self.text_rasterizer.measure_text(text, gs, &self.fonts)
                    };

                    // The rasterizer returns a scalar magnitude along the
                    // active writing axis. advance_text_matrix routes it
                    // to x (WMode 0) or y (WMode 1), keeping the axis
                    // swap in exactly one place.
                    gs_stack.current_mut().advance_text_matrix(advance);
                }
            }
            Operator::Quote { text } => {
                if in_text_object {
                    // Quote (') is T* followed by Tj — always advance line
                    let gs_mut = gs_stack.current_mut();
                    let leading = gs_mut.leading;
                    let translation = Matrix::translation(0.0, -leading);
                    gs_mut.text_line_matrix = translation.multiply(&gs_mut.text_line_matrix);
                    gs_mut.text_matrix = gs_mut.text_line_matrix;

                    // Type 3 glyphs are painted via the content-stream renderer.
                    if self.current_font_is_type3(gs_stack.current()) {
                        let advance = if excluded_layer_depth == 0 {
                            let gs_snap = gs_stack.current().clone();
                            self.render_type3_text(
                                pixmap,
                                text,
                                base_transform,
                                &gs_snap,
                                doc,
                                page_num,
                                resources,
                            )?
                        } else {
                            self.text_rasterizer
                                .measure_text(text, gs_stack.current(), &self.fonts)
                        };
                        gs_stack.current_mut().advance_text_matrix(advance);
                        return Ok(true);
                    }

                    let gs = gs_stack.current();
                    let advance = if excluded_layer_depth == 0 {
                        let clip = clip_stack.last().and_then(|c| c.as_ref());
                        let transform = combine_transforms(base_transform, &gs.ctm);
                        log::debug!(
                            "' (Quote): rendering text at Tm=[{}, {}, {}, {}, {}, {}]",
                            gs.text_matrix.a,
                            gs.text_matrix.b,
                            gs.text_matrix.c,
                            gs.text_matrix.d,
                            gs.text_matrix.e,
                            gs.text_matrix.f
                        );
                        // WS1.5b — accumulate clip-mode glyph outlines.
                        if gs.render_mode >= 4 {
                            self.accumulate_text_clip_tj(
                                text_clip_accum,
                                pixmap.width(),
                                pixmap.height(),
                                text,
                                transform,
                                gs,
                                resources,
                                doc,
                            );
                        }
                        // Same shape as `Tj`. `'` is `T* Tj` per
                        // ISO 32000-1; the resolved colour depends only
                        // on the prior colour-setting ops, so the resolve
                        // happens here, not inside `T*`.
                        let colors = self.pipeline_resolve_text_colors(doc, gs);
                        let smask_snap = self.smask_snapshot(pixmap, gs);
                        let smask_spot_snap = self.smask_spot_snapshot(gs);
                        let overprint_snap = self.overprint_snapshot(pixmap, gs, true);
                        let cmyk_compose_snap = self.cmyk_compose_snapshot(pixmap, gs, doc, true);
                        let spot_snap = self.text_fill_spot_snapshot(pixmap, gs);
                        let text_coverage = spot_snap.as_ref().and_then(|_| {
                            self.rasterise_text_coverage_render_text(
                                text, transform, gs, resources, doc, clip,
                            )
                        });
                        let adv = self.text_rasterizer.render_text(
                            pixmap,
                            text,
                            transform,
                            gs,
                            colors.as_ref(),
                            resources,
                            doc,
                            clip,
                            &self.fonts,
                        )?;
                        let gs_for_apply = gs_stack.current().clone();
                        if let Some(snap) = cmyk_compose_snap {
                            self.apply_cmyk_compose_after_paint(
                                pixmap,
                                &snap,
                                &gs_for_apply,
                                doc,
                                true,
                            );
                        }
                        if let Some(snap) = overprint_snap {
                            self.apply_overprint_after_paint(
                                pixmap,
                                &snap,
                                &gs_for_apply,
                                doc,
                                true,
                            );
                        }
                        if let Some(snap) = spot_snap {
                            self.mirror_spot_paint_into_sidecar_with_coverage(
                                pixmap,
                                &snap,
                                text_coverage.as_deref(),
                                &gs_for_apply,
                                true,
                            );
                        }
                        if let Some(snap) = smask_snap {
                            self.apply_smask_after_paint(
                                pixmap,
                                &snap,
                                smask_spot_snap.as_deref(),
                                &gs_for_apply,
                                doc,
                                page_num,
                                resources,
                                base_transform,
                            )?;
                        }
                        adv
                    } else {
                        self.text_rasterizer.measure_text(text, gs, &self.fonts)
                    };

                    // The rasterizer returns a scalar magnitude along the
                    // active writing axis. advance_text_matrix routes it
                    // to x (WMode 0) or y (WMode 1), keeping the axis
                    // swap in exactly one place.
                    gs_stack.current_mut().advance_text_matrix(advance);
                }
            }
            Operator::TJ { array } => {
                if in_text_object {
                    // Type 3 glyphs are painted via the content-stream renderer.
                    if self.current_font_is_type3(gs_stack.current()) {
                        let advance = if excluded_layer_depth == 0 {
                            let gs_snap = gs_stack.current().clone();
                            self.render_type3_tj_array(
                                pixmap,
                                array,
                                base_transform,
                                &gs_snap,
                                doc,
                                page_num,
                                resources,
                            )?
                        } else {
                            self.text_rasterizer.measure_tj_array(
                                array,
                                gs_stack.current(),
                                &self.fonts,
                            )
                        };
                        gs_stack.current_mut().advance_text_matrix(advance);
                        return Ok(true);
                    }
                    let gs = gs_stack.current();
                    let advance = if excluded_layer_depth == 0 {
                        let clip = clip_stack.last().and_then(|c| c.as_ref());
                        let transform = combine_transforms(base_transform, &gs.ctm);
                        log::debug!(
                            "TJ: rendering array at Tm=[{}, {}, {}, {}, {}, {}]",
                            gs.text_matrix.a,
                            gs.text_matrix.b,
                            gs.text_matrix.c,
                            gs.text_matrix.d,
                            gs.text_matrix.e,
                            gs.text_matrix.f
                        );
                        // WS1.5b — accumulate clip-mode glyph outlines
                        // (Tr 4–7) for the whole positioning array.
                        if gs.render_mode >= 4 {
                            self.accumulate_text_clip_tj_array(
                                text_clip_accum,
                                pixmap.width(),
                                pixmap.height(),
                                array,
                                transform,
                                gs,
                                resources,
                                doc,
                            );
                        }
                        // Resolve once for the whole `TJ` array — the
                        // numeric offsets inside `array` only adjust
                        // positioning; they cannot alter the active
                        // colour mid-string. The rasteriser threads the
                        // override into the per-element `render_text`
                        // calls so the colour propagates without an
                        // operator-arm-side clone of `gs`.
                        let colors = self.pipeline_resolve_text_colors(doc, gs);
                        let smask_snap = self.smask_snapshot(pixmap, gs);
                        let smask_spot_snap = self.smask_spot_snapshot(gs);
                        let overprint_snap = self.overprint_snapshot(pixmap, gs, true);
                        let cmyk_compose_snap = self.cmyk_compose_snapshot(pixmap, gs, doc, true);
                        let spot_snap = self.text_fill_spot_snapshot(pixmap, gs);
                        let text_coverage = spot_snap.as_ref().and_then(|_| {
                            self.rasterise_text_coverage_render_tj_array(
                                array, transform, gs, resources, doc, clip,
                            )
                        });
                        let adv = self.text_rasterizer.render_tj_array(
                            pixmap,
                            array,
                            transform,
                            gs,
                            colors.as_ref(),
                            resources,
                            doc,
                            clip,
                            &self.fonts,
                        )?;
                        let gs_for_apply = gs_stack.current().clone();
                        if let Some(snap) = cmyk_compose_snap {
                            self.apply_cmyk_compose_after_paint(
                                pixmap,
                                &snap,
                                &gs_for_apply,
                                doc,
                                true,
                            );
                        }
                        if let Some(snap) = overprint_snap {
                            self.apply_overprint_after_paint(
                                pixmap,
                                &snap,
                                &gs_for_apply,
                                doc,
                                true,
                            );
                        }
                        if let Some(snap) = spot_snap {
                            self.mirror_spot_paint_into_sidecar_with_coverage(
                                pixmap,
                                &snap,
                                text_coverage.as_deref(),
                                &gs_for_apply,
                                true,
                            );
                        }
                        if let Some(snap) = smask_snap {
                            self.apply_smask_after_paint(
                                pixmap,
                                &snap,
                                smask_spot_snap.as_deref(),
                                &gs_for_apply,
                                doc,
                                page_num,
                                resources,
                                base_transform,
                            )?;
                        }
                        adv
                    } else {
                        self.text_rasterizer
                            .measure_tj_array(array, gs, &self.fonts)
                    };

                    // The rasterizer returns a scalar magnitude along the
                    // active writing axis. advance_text_matrix routes it
                    // to x (WMode 0) or y (WMode 1), keeping the axis
                    // swap in exactly one place.
                    gs_stack.current_mut().advance_text_matrix(advance);
                }
            }
            Operator::DoubleQuote {
                word_space,
                char_space,
                text,
            } => {
                if in_text_object {
                    // Double Quote (") always updates state
                    let gs_mut = gs_stack.current_mut();
                    gs_mut.word_space = *word_space;
                    gs_mut.char_space = *char_space;

                    let leading = gs_mut.leading;
                    let translation = Matrix::translation(0.0, -leading);
                    gs_mut.text_line_matrix = translation.multiply(&gs_mut.text_line_matrix);
                    gs_mut.text_matrix = gs_mut.text_line_matrix;

                    // Type 3 glyphs are painted via the content-stream renderer.
                    if self.current_font_is_type3(gs_stack.current()) {
                        let advance = if excluded_layer_depth == 0 {
                            let gs_snap = gs_stack.current().clone();
                            self.render_type3_text(
                                pixmap,
                                text,
                                base_transform,
                                &gs_snap,
                                doc,
                                page_num,
                                resources,
                            )?
                        } else {
                            self.text_rasterizer
                                .measure_text(text, gs_stack.current(), &self.fonts)
                        };
                        gs_stack.current_mut().advance_text_matrix(advance);
                        return Ok(true);
                    }

                    let gs = gs_stack.current();
                    let advance = if excluded_layer_depth == 0 {
                        let clip = clip_stack.last().and_then(|c| c.as_ref());
                        let transform = combine_transforms(base_transform, &gs.ctm);
                        log::debug!(
                            "\" (DoubleQuote): rendering text at Tm=[{}, {}, {}, {}, {}, {}]",
                            gs.text_matrix.a,
                            gs.text_matrix.b,
                            gs.text_matrix.c,
                            gs.text_matrix.d,
                            gs.text_matrix.e,
                            gs.text_matrix.f
                        );
                        // WS1.5b — accumulate clip-mode glyph outlines.
                        if gs.render_mode >= 4 {
                            self.accumulate_text_clip_tj(
                                text_clip_accum,
                                pixmap.width(),
                                pixmap.height(),
                                text,
                                transform,
                                gs,
                                resources,
                                doc,
                            );
                        }
                        // `"` is equivalent to setting Tw, Tc, then
                        // `T* Tj`. Tw/Tc are state-only and don't
                        // influence the resolved colour, so the resolve
                        // happens immediately before painting just like
                        // in `Tj` / `'`.
                        let colors = self.pipeline_resolve_text_colors(doc, gs);
                        let smask_snap = self.smask_snapshot(pixmap, gs);
                        let smask_spot_snap = self.smask_spot_snapshot(gs);
                        let overprint_snap = self.overprint_snapshot(pixmap, gs, true);
                        let cmyk_compose_snap = self.cmyk_compose_snapshot(pixmap, gs, doc, true);
                        let spot_snap = self.text_fill_spot_snapshot(pixmap, gs);
                        let text_coverage = spot_snap.as_ref().and_then(|_| {
                            self.rasterise_text_coverage_render_text(
                                text, transform, gs, resources, doc, clip,
                            )
                        });
                        let adv = self.text_rasterizer.render_text(
                            pixmap,
                            text,
                            transform,
                            gs,
                            colors.as_ref(),
                            resources,
                            doc,
                            clip,
                            &self.fonts,
                        )?;
                        let gs_for_apply = gs_stack.current().clone();
                        if let Some(snap) = cmyk_compose_snap {
                            self.apply_cmyk_compose_after_paint(
                                pixmap,
                                &snap,
                                &gs_for_apply,
                                doc,
                                true,
                            );
                        }
                        if let Some(snap) = overprint_snap {
                            self.apply_overprint_after_paint(
                                pixmap,
                                &snap,
                                &gs_for_apply,
                                doc,
                                true,
                            );
                        }
                        if let Some(snap) = spot_snap {
                            self.mirror_spot_paint_into_sidecar_with_coverage(
                                pixmap,
                                &snap,
                                text_coverage.as_deref(),
                                &gs_for_apply,
                                true,
                            );
                        }
                        if let Some(snap) = smask_snap {
                            self.apply_smask_after_paint(
                                pixmap,
                                &snap,
                                smask_spot_snap.as_deref(),
                                &gs_for_apply,
                                doc,
                                page_num,
                                resources,
                                base_transform,
                            )?;
                        }
                        adv
                    } else {
                        self.text_rasterizer.measure_text(text, gs, &self.fonts)
                    };

                    // The rasterizer returns a scalar magnitude along the
                    // active writing axis. advance_text_matrix routes it
                    // to x (WMode 0) or y (WMode 1), keeping the axis
                    // swap in exactly one place.
                    gs_stack.current_mut().advance_text_matrix(advance);
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }
}
