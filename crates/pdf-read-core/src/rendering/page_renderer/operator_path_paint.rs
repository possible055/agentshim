use super::*;

impl PageRenderer {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn execute_path_paint_operator(
        &mut self,
        op: &Operator,
        pixmap: &mut Pixmap,
        base_transform: Transform,
        current_path: &mut PathBuilder,
        pending_clip: &mut Option<(tiny_skia::Path, tiny_skia::FillRule)>,
        clip_stack: &mut Vec<Option<tiny_skia::Mask>>,
        gs_stack: &mut GraphicsStateStack,
        excluded_layer_depth: u32,
        doc: &PdfDocument,
        page_num: usize,
        resources: &Object,
    ) -> Result<bool> {
        match op {
            Operator::Stroke => {
                if excluded_layer_depth == 0 {
                    apply_pending_clip(pending_clip, clip_stack, pixmap, base_transform, gs_stack);
                    let clip = clip_stack.last().and_then(|c| c.as_ref());
                    if let Some(path) = std::mem::replace(current_path, PathBuilder::new()).finish()
                    {
                        let gs_clone = gs_stack.current().clone();
                        // Stroke side mirrors the path-fill routing —
                        // route through the pipeline so Type 4 Separation
                        // strokes resolve correctly. Line width / cap /
                        // join / dash come from the cloned `gs`
                        // unchanged, so the stroke geometry is unaffected
                        // by the colour splice.
                        let spliced = self.pipeline_resolve_paint_gs(
                            doc,
                            &gs_clone,
                            PipelinePaintKind::PathStroke,
                        );
                        let render_gs: &GraphicsState = spliced.as_ref().unwrap_or(&gs_clone);
                        let transform = combine_transforms(base_transform, &gs_clone.ctm);
                        let smask_snap = self.smask_snapshot(pixmap, &gs_clone);
                        let smask_spot_snap = self.smask_spot_snapshot(&gs_clone);
                        let overprint_snap = self.overprint_snapshot(pixmap, &gs_clone, false);
                        let cmyk_compose_snap =
                            self.cmyk_compose_snapshot(pixmap, &gs_clone, doc, false);
                        let cmyk_sidecar_snap =
                            self.cmyk_sidecar_snapshot(pixmap, &gs_clone, false);
                        let rgb_sidecar_snap =
                            self.cmyk_sidecar_snapshot_for_rgb_paint(pixmap, &gs_clone, false);
                        let cmyk_coverage =
                            self.rasterise_stroke_coverage(&path, transform, &gs_clone, clip);
                        self.path_rasterizer
                            .stroke_path_clipped(pixmap, &path, transform, render_gs, clip);
                        if let Some(snap) = cmyk_compose_snap {
                            self.apply_cmyk_compose_after_paint_with_coverage(
                                pixmap,
                                &snap,
                                cmyk_coverage.as_deref(),
                                &gs_clone,
                                doc,
                                false,
                            );
                        }
                        if let Some(snap) = overprint_snap {
                            self.apply_overprint_after_paint_with_coverage(
                                pixmap,
                                &snap,
                                cmyk_coverage.as_deref(),
                                &gs_clone,
                                doc,
                                false,
                            );
                        }
                        if let Some(snap) = cmyk_sidecar_snap {
                            self.mirror_cmyk_paint_into_sidecar_with_coverage(
                                pixmap,
                                &snap,
                                cmyk_coverage.as_deref(),
                                &gs_clone,
                                doc,
                                false,
                            );
                        }
                        if let Some(snap) = rgb_sidecar_snap {
                            self.mirror_rgb_paint_into_sidecar_with_coverage(
                                pixmap,
                                &snap,
                                cmyk_coverage.as_deref(),
                                &gs_clone,
                                doc,
                                false,
                            );
                        }
                        self.mirror_spot_paint_into_sidecar_with_coverage(
                            pixmap,
                            &[],
                            cmyk_coverage.as_deref(),
                            &gs_clone,
                            false,
                        );
                        if let Some(snap) = smask_snap {
                            self.apply_smask_after_paint(
                                pixmap,
                                &snap,
                                smask_spot_snap.as_deref(),
                                &gs_clone,
                                doc,
                                page_num,
                                resources,
                                base_transform,
                            )?;
                        }
                    }
                } else {
                    let _ = std::mem::replace(current_path, PathBuilder::new()).finish();
                }
                *current_path = PathBuilder::new();
            }
            Operator::Fill => {
                if excluded_layer_depth == 0 {
                    apply_pending_clip(pending_clip, clip_stack, pixmap, base_transform, gs_stack);
                    let clip = clip_stack.last().and_then(|c| c.as_ref());
                    if let Some(path) = std::mem::replace(current_path, PathBuilder::new()).finish()
                    {
                        let gs_clone = gs_stack.current().clone();
                        // Resolve the active fill colour through the
                        // pipeline (PostScript Type 4 tint transforms,
                        // ICCBased N=4, etc.) and splice the resulting
                        // RGBA into a transient GraphicsState copy the
                        // rasteriser consumes.
                        let spliced = self.pipeline_resolve_paint_gs(
                            doc,
                            &gs_clone,
                            PipelinePaintKind::PathFill,
                        );
                        let render_gs: &GraphicsState = spliced.as_ref().unwrap_or(&gs_clone);
                        let transform = combine_transforms(base_transform, &gs_clone.ctm);
                        // §8.7.3: a Pattern-space fill routes to the
                        // tiling-pattern rasteriser first. When it paints
                        // the region the solid-colour paint below is
                        // skipped; unsupported/shading patterns return
                        // false and fall through to the solid fallback.
                        if gs_clone.fill_color_space == "Pattern"
                            && gs_clone.fill_pattern_name.is_some()
                            && self.fill_with_tiling_pattern(
                                pixmap,
                                &path,
                                base_transform,
                                transform,
                                tiny_skia::FillRule::Winding,
                                clip,
                                &gs_clone,
                                doc,
                                page_num,
                                resources,
                            )?
                        {
                            // Painted by the tiling pattern.
                        } else {
                            // §11.4.7 + §11.7.4: snapshot before the
                            // paint so the post-paint modulators can
                            // blend the backdrop (snapshot) with the
                            // painted result.
                            let smask_snap = self.smask_snapshot(pixmap, &gs_clone);
                            let smask_spot_snap = self.smask_spot_snapshot(&gs_clone);
                            let overprint_snap = self.overprint_snapshot(pixmap, &gs_clone, true);
                            let cmyk_compose_snap =
                                self.cmyk_compose_snapshot(pixmap, &gs_clone, doc, true);
                            let cmyk_sidecar_snap =
                                self.cmyk_sidecar_snapshot(pixmap, &gs_clone, true);
                            let rgb_sidecar_snap =
                                self.cmyk_sidecar_snapshot_for_rgb_paint(pixmap, &gs_clone, true);
                            let cmyk_coverage = self.rasterise_fill_coverage(
                                &path,
                                transform,
                                tiny_skia::FillRule::Winding,
                                clip,
                            );
                            self.path_rasterizer.fill_path_clipped(
                                pixmap,
                                &path,
                                transform,
                                render_gs,
                                tiny_skia::FillRule::Winding,
                                clip,
                            );
                            if let Some(snap) = cmyk_compose_snap {
                                self.apply_cmyk_compose_after_paint_with_coverage(
                                    pixmap,
                                    &snap,
                                    cmyk_coverage.as_deref(),
                                    &gs_clone,
                                    doc,
                                    true,
                                );
                            }
                            if let Some(snap) = overprint_snap {
                                self.apply_overprint_after_paint_with_coverage(
                                    pixmap,
                                    &snap,
                                    cmyk_coverage.as_deref(),
                                    &gs_clone,
                                    doc,
                                    true,
                                );
                            }
                            if let Some(snap) = cmyk_sidecar_snap {
                                self.mirror_cmyk_paint_into_sidecar_with_coverage(
                                    pixmap,
                                    &snap,
                                    cmyk_coverage.as_deref(),
                                    &gs_clone,
                                    doc,
                                    true,
                                );
                            }
                            if let Some(snap) = rgb_sidecar_snap {
                                self.mirror_rgb_paint_into_sidecar_with_coverage(
                                    pixmap,
                                    &snap,
                                    cmyk_coverage.as_deref(),
                                    &gs_clone,
                                    doc,
                                    true,
                                );
                            }
                            self.mirror_spot_paint_into_sidecar_with_coverage(
                                pixmap,
                                &[],
                                cmyk_coverage.as_deref(),
                                &gs_clone,
                                true,
                            );
                            if let Some(snap) = smask_snap {
                                self.apply_smask_after_paint(
                                    pixmap,
                                    &snap,
                                    smask_spot_snap.as_deref(),
                                    &gs_clone,
                                    doc,
                                    page_num,
                                    resources,
                                    base_transform,
                                )?;
                            }
                        }
                    }
                } else {
                    let _ = std::mem::replace(current_path, PathBuilder::new()).finish();
                }
                *current_path = PathBuilder::new();
            }
            Operator::FillStroke | Operator::CloseFillStroke | Operator::CloseFillStrokeEvenOdd => {
                if excluded_layer_depth == 0 {
                    apply_pending_clip(pending_clip, clip_stack, pixmap, base_transform, gs_stack);
                    let clip = clip_stack.last().and_then(|c| c.as_ref());
                    // ISO 32000-1 §8.5.3.1 Table 60: `b` and `b*` close
                    // the path before fill+stroke. The parser does not
                    // decompose them (unlike `s`, which is emitted as
                    // `ClosePath` + `Stroke`), so the dispatcher must
                    // perform the close itself or the final segment of
                    // an open subpath will not be painted by the stroke.
                    if matches!(
                        op,
                        Operator::CloseFillStroke | Operator::CloseFillStrokeEvenOdd
                    ) {
                        current_path.close();
                    }
                    if let Some(path) = std::mem::replace(current_path, PathBuilder::new()).finish()
                    {
                        let gs_clone = gs_stack.current().clone();
                        let transform = combine_transforms(base_transform, &gs_clone.ctm);
                        let fill_rule = if matches!(op, Operator::CloseFillStrokeEvenOdd) {
                            tiny_skia::FillRule::EvenOdd
                        } else {
                            tiny_skia::FillRule::Winding
                        };
                        // Combos resolve fill and stroke independently
                        // through the pipeline (two `PaintIntent`s per
                        // operator). Each side falls back to the
                        // GraphicsState's existing RGBA if its colour
                        // can't be resolved, so a Type 4 Separation on
                        // the fill side and a plain DeviceRGB on the
                        // stroke side route correctly without
                        // entangling the two.
                        //
                        // Single splice for both sides — the rasteriser
                        // reads fill fields for the fill pass and stroke
                        // fields for the stroke pass, so one clone with
                        // both sides written is equivalent to two
                        // single-side clones.
                        let spliced = self.pipeline_resolve_paint_gs(
                            doc,
                            &gs_clone,
                            PipelinePaintKind::PathFillStroke,
                        );
                        let render_gs: &GraphicsState = spliced.as_ref().unwrap_or(&gs_clone);

                        // §8.7.3: Pattern-space fills route to the tiling
                        // rasteriser first; on success the solid fill side
                        // is skipped (the stroke side still runs below).
                        let fill_by_pattern = gs_clone.fill_color_space == "Pattern"
                            && gs_clone.fill_pattern_name.is_some()
                            && self.fill_with_tiling_pattern(
                                pixmap,
                                &path,
                                base_transform,
                                transform,
                                fill_rule,
                                clip,
                                &gs_clone,
                                doc,
                                page_num,
                                resources,
                            )?;

                        // Fill side: snapshot before paint, paint,
                        // then run compose-first / overprint / SMask
                        // correctors against the fill-side gs fields.
                        // The §11.7.4 + §11.4.7 + §11.4 rules apply
                        // to combos exactly as they do to plain `f`
                        // — the only difference here is the stroke
                        // pass also lays paint on top, so each side
                        // gets its own snapshot/apply cycle.
                        if !fill_by_pattern {
                            let fill_smask_snap = self.smask_snapshot(pixmap, &gs_clone);
                            let fill_smask_spot_snap = self.smask_spot_snapshot(&gs_clone);
                            let fill_overprint_snap =
                                self.overprint_snapshot(pixmap, &gs_clone, true);
                            let fill_cmyk_compose_snap =
                                self.cmyk_compose_snapshot(pixmap, &gs_clone, doc, true);
                            let fill_spot_snap = self.spot_paint_snapshot(pixmap, &gs_clone, true);
                            // §11.7.3 + §11.3.3 require per-pixel
                            // coverage on every lane. The path-Fill
                            // helper uses `rasterise_fill_coverage`;
                            // the combo arm uses the same call so AA
                            // edges receive fractional coverage and an
                            // alternate-CS RGB collision with backdrop
                            // does not mask the paint from the spot
                            // mirror's diff branch.
                            let fill_cmyk_coverage =
                                self.rasterise_fill_coverage(&path, transform, fill_rule, clip);
                            self.path_rasterizer.fill_path_clipped(
                                pixmap, &path, transform, render_gs, fill_rule, clip,
                            );
                            if let Some(snap) = fill_cmyk_compose_snap {
                                self.apply_cmyk_compose_after_paint(
                                    pixmap, &snap, &gs_clone, doc, true,
                                );
                            }
                            if let Some(snap) = fill_overprint_snap {
                                self.apply_overprint_after_paint(
                                    pixmap, &snap, &gs_clone, doc, true,
                                );
                            }
                            if let Some(snap) = fill_spot_snap {
                                self.mirror_spot_paint_into_sidecar_with_coverage(
                                    pixmap,
                                    &snap,
                                    fill_cmyk_coverage.as_deref(),
                                    &gs_clone,
                                    true,
                                );
                            }
                            if let Some(snap) = fill_smask_snap {
                                self.apply_smask_after_paint(
                                    pixmap,
                                    &snap,
                                    fill_smask_spot_snap.as_deref(),
                                    &gs_clone,
                                    doc,
                                    page_num,
                                    resources,
                                    base_transform,
                                )?;
                            }
                        }

                        // Stroke side: same snapshot/apply pattern
                        // against the stroke-side fields.
                        let stroke_smask_snap = self.smask_snapshot(pixmap, &gs_clone);
                        let stroke_smask_spot_snap = self.smask_spot_snapshot(&gs_clone);
                        let stroke_overprint_snap =
                            self.overprint_snapshot(pixmap, &gs_clone, false);
                        let stroke_cmyk_compose_snap =
                            self.cmyk_compose_snapshot(pixmap, &gs_clone, doc, false);
                        let stroke_spot_snap = self.spot_paint_snapshot(pixmap, &gs_clone, false);
                        let stroke_cmyk_coverage =
                            self.rasterise_stroke_coverage(&path, transform, &gs_clone, clip);
                        self.path_rasterizer
                            .stroke_path_clipped(pixmap, &path, transform, render_gs, clip);
                        if let Some(snap) = stroke_cmyk_compose_snap {
                            self.apply_cmyk_compose_after_paint(
                                pixmap, &snap, &gs_clone, doc, false,
                            );
                        }
                        if let Some(snap) = stroke_overprint_snap {
                            self.apply_overprint_after_paint(pixmap, &snap, &gs_clone, doc, false);
                        }
                        if let Some(snap) = stroke_spot_snap {
                            self.mirror_spot_paint_into_sidecar_with_coverage(
                                pixmap,
                                &snap,
                                stroke_cmyk_coverage.as_deref(),
                                &gs_clone,
                                false,
                            );
                        }
                        if let Some(snap) = stroke_smask_snap {
                            self.apply_smask_after_paint(
                                pixmap,
                                &snap,
                                stroke_smask_spot_snap.as_deref(),
                                &gs_clone,
                                doc,
                                page_num,
                                resources,
                                base_transform,
                            )?;
                        }
                    }
                } else {
                    let _ = std::mem::replace(current_path, PathBuilder::new()).finish();
                }
                *current_path = PathBuilder::new();
            }
            Operator::FillEvenOdd | Operator::FillStrokeEvenOdd => {
                if excluded_layer_depth == 0 {
                    apply_pending_clip(pending_clip, clip_stack, pixmap, base_transform, gs_stack);
                    let clip = clip_stack.last().and_then(|c| c.as_ref());
                    if let Some(path) = std::mem::replace(current_path, PathBuilder::new()).finish()
                    {
                        let gs_clone = gs_stack.current().clone();
                        let transform = combine_transforms(base_transform, &gs_clone.ctm);
                        // One unified resolve covers both fill and the
                        // optional stroke pass — for plain `f*` the
                        // helper produces a fill-only splice; for
                        // `B*`/`b*` both sides are spliced into the
                        // same clone. Either way, the rasteriser reads
                        // the side it needs from `render_gs`.
                        let kind = if matches!(op, Operator::FillStrokeEvenOdd) {
                            PipelinePaintKind::PathFillStroke
                        } else {
                            PipelinePaintKind::PathFill
                        };
                        let spliced = self.pipeline_resolve_paint_gs(doc, &gs_clone, kind);
                        let render_gs: &GraphicsState = spliced.as_ref().unwrap_or(&gs_clone);

                        // §8.7.3: Pattern-space fills route to the tiling
                        // rasteriser first; on success the solid fill side
                        // is skipped (the stroke side, if any, still runs).
                        let fill_by_pattern = gs_clone.fill_color_space == "Pattern"
                            && gs_clone.fill_pattern_name.is_some()
                            && self.fill_with_tiling_pattern(
                                pixmap,
                                &path,
                                base_transform,
                                transform,
                                tiny_skia::FillRule::EvenOdd,
                                clip,
                                &gs_clone,
                                doc,
                                page_num,
                                resources,
                            )?;

                        // Fill side: snapshot + paint + correctors.
                        // §11.4.7 + §11.7.4 + §11.4 compose-first
                        // each apply to `f*` just as they do to `f`
                        // — the only difference is the EvenOdd fill
                        // rule, which only changes coverage, not
                        // the colour-composition rule.
                        if !fill_by_pattern {
                            let fill_smask_snap = self.smask_snapshot(pixmap, &gs_clone);
                            let fill_smask_spot_snap = self.smask_spot_snapshot(&gs_clone);
                            let fill_overprint_snap =
                                self.overprint_snapshot(pixmap, &gs_clone, true);
                            let fill_cmyk_compose_snap =
                                self.cmyk_compose_snapshot(pixmap, &gs_clone, doc, true);
                            let fill_spot_snap = self.spot_paint_snapshot(pixmap, &gs_clone, true);
                            // §11.7.3 + §11.3.3 spot mirror needs a
                            // real per-pixel coverage mask — see the
                            // FillStroke arm above for the rationale.
                            let fill_cmyk_coverage = self.rasterise_fill_coverage(
                                &path,
                                transform,
                                tiny_skia::FillRule::EvenOdd,
                                clip,
                            );
                            self.path_rasterizer.fill_path_clipped(
                                pixmap,
                                &path,
                                transform,
                                render_gs,
                                tiny_skia::FillRule::EvenOdd,
                                clip,
                            );
                            if let Some(snap) = fill_cmyk_compose_snap {
                                self.apply_cmyk_compose_after_paint(
                                    pixmap, &snap, &gs_clone, doc, true,
                                );
                            }
                            if let Some(snap) = fill_overprint_snap {
                                self.apply_overprint_after_paint(
                                    pixmap, &snap, &gs_clone, doc, true,
                                );
                            }
                            if let Some(snap) = fill_spot_snap {
                                self.mirror_spot_paint_into_sidecar_with_coverage(
                                    pixmap,
                                    &snap,
                                    fill_cmyk_coverage.as_deref(),
                                    &gs_clone,
                                    true,
                                );
                            }
                            if let Some(snap) = fill_smask_snap {
                                self.apply_smask_after_paint(
                                    pixmap,
                                    &snap,
                                    fill_smask_spot_snap.as_deref(),
                                    &gs_clone,
                                    doc,
                                    page_num,
                                    resources,
                                    base_transform,
                                )?;
                            }
                        }

                        if matches!(op, Operator::FillStrokeEvenOdd) {
                            // Stroke side: same snapshot/paint/apply
                            // cycle against the stroke fields.
                            let stroke_smask_snap = self.smask_snapshot(pixmap, &gs_clone);
                            let stroke_smask_spot_snap = self.smask_spot_snapshot(&gs_clone);
                            let stroke_overprint_snap =
                                self.overprint_snapshot(pixmap, &gs_clone, false);
                            let stroke_cmyk_compose_snap =
                                self.cmyk_compose_snapshot(pixmap, &gs_clone, doc, false);
                            let stroke_spot_snap =
                                self.spot_paint_snapshot(pixmap, &gs_clone, false);
                            let stroke_cmyk_coverage =
                                self.rasterise_stroke_coverage(&path, transform, &gs_clone, clip);
                            self.path_rasterizer
                                .stroke_path_clipped(pixmap, &path, transform, render_gs, clip);
                            if let Some(snap) = stroke_cmyk_compose_snap {
                                self.apply_cmyk_compose_after_paint(
                                    pixmap, &snap, &gs_clone, doc, false,
                                );
                            }
                            if let Some(snap) = stroke_overprint_snap {
                                self.apply_overprint_after_paint(
                                    pixmap, &snap, &gs_clone, doc, false,
                                );
                            }
                            if let Some(snap) = stroke_spot_snap {
                                self.mirror_spot_paint_into_sidecar_with_coverage(
                                    pixmap,
                                    &snap,
                                    stroke_cmyk_coverage.as_deref(),
                                    &gs_clone,
                                    false,
                                );
                            }
                            if let Some(snap) = stroke_smask_snap {
                                self.apply_smask_after_paint(
                                    pixmap,
                                    &snap,
                                    stroke_smask_spot_snap.as_deref(),
                                    &gs_clone,
                                    doc,
                                    page_num,
                                    resources,
                                    base_transform,
                                )?;
                            }
                        }
                    }
                } else {
                    let _ = std::mem::replace(current_path, PathBuilder::new()).finish();
                }
                *current_path = PathBuilder::new();
            }
            _ => return Ok(false),
        }
        Ok(true)
    }
}
