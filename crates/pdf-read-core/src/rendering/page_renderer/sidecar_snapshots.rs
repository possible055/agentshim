use super::*;

impl PageRenderer {
    /// Take a snapshot of `pixmap` if the graphics state has an active
    /// `/SMask`. The caller paints normally, then calls
    /// [`Self::apply_smask_after_paint`] with the snapshot to modulate
    /// the painted contribution by the soft mask. Returns `None` when
    /// the gs has no soft mask, so the caller takes the no-op branch.
    pub(super) fn smask_snapshot(&self, pixmap: &Pixmap, gs: &GraphicsState) -> Option<Vec<u8>> {
        if gs.smask.is_some() {
            Some(pixmap.data().to_vec())
        } else {
            None
        }
    }

    /// Companion to [`Self::smask_snapshot`] for the spot-lane sidecar.
    /// When the graphics state has an active `/SMask` AND the sidecar
    /// is allocated, return a flat snapshot of every spot plane so the
    /// SMask attenuation path can blend `m·post_mirror + (1-m)·pre`
    /// per pixel per lane.
    ///
    /// ISO 32000-1 §11.3.3 + §11.7.3: "Only a single shape value and
    /// opacity value shall be maintained at each point in the computed
    /// group results; they shall apply to both process and spot colour
    /// components." The pixmap's RGB lanes receive the SMask alpha
    /// attenuation via [`Self::apply_smask_after_paint`]; the spot
    /// lanes need the same attenuation against their pre-paint state so
    /// the lane composes at the spec-correct effective alpha.
    pub(super) fn smask_spot_snapshot(&self, gs: &GraphicsState) -> Option<Vec<u8>> {
        gs.smask.as_ref()?;
        let sidecar = self.cmyk_sidecar.as_ref()?;
        Some(sidecar.spots_all().to_vec())
    }

    /// Predicate: should the CMYK compose-before-convert path fire for
    /// the current paint operator? Per ISO 32000-1:2008 §11.4 + Annex G,
    /// transparency compositing happens in the source colour space and
    /// the OutputIntent ICC conversion happens at display. When all of
    /// the following hold, the spec-correct rendering requires composing
    /// in CMYK before converting through the ICC profile:
    ///
    /// * The active colour on the relevant side is genuine CMYK
    ///   (`gs.fill_color_cmyk` / `gs.stroke_color_cmyk` populated).
    /// * The graphics state declares non-trivial transparency: alpha
    ///   below 1.0, a non-Normal blend mode, or an active soft mask.
    /// * A CMYK OutputIntent ICC profile is available (otherwise the
    ///   additive-clamp fallback is linear, so convert-first and
    ///   compose-first are byte-identical and we save the work).
    ///
    /// Returns `true` only when every condition is met so the no-op
    /// branch is the cheapest possible test: a single ICC-profile
    /// lookup + a few `gs` field reads.
    pub(super) fn cmyk_compose_active(
        &self,
        gs: &GraphicsState,
        doc: &PdfDocument,
        fill_side: bool,
    ) -> bool {
        let has_cmyk = if fill_side {
            gs.fill_color_cmyk.is_some()
        } else {
            gs.stroke_color_cmyk.is_some()
        };
        if !has_cmyk {
            return false;
        }
        // ISO 32000-1 §11.7.4.3: when overprint is active the
        // CompatibleOverprint blend function takes over the per-channel
        // composition (`α · B(c_b, c_s) + (1 - α) · c_b`). Running the
        // compose-first helper additionally would double-touch the
        // sidecar and corrupt the OPM=1 preserve-on-zero rule (compose
        // would write `(1-α)·c_b`, then overprint would read that as
        // the new backdrop). The overprint helper handles compose
        // itself for overprint paints.
        let overprint = if fill_side {
            gs.fill_overprint
        } else {
            gs.stroke_overprint
        };
        if overprint {
            return false;
        }
        let alpha = if fill_side {
            gs.fill_alpha
        } else {
            gs.stroke_alpha
        };
        let non_trivial = alpha < 1.0 || gs.blend_mode != "Normal" || gs.smask.is_some();
        if !non_trivial {
            return false;
        }
        doc.output_intent_cmyk_profile().is_some()
    }

    /// Snapshot the pixmap when [`Self::cmyk_compose_active`] returns
    /// true. The caller paints normally with the tiny_skia rasteriser
    /// (which renders CMYK→RGB-via-ICC then alpha-blends in RGB — the
    /// convert-first path), then hands the snapshot to
    /// [`Self::apply_cmyk_compose_after_paint`] to overwrite the
    /// painted region with the compose-first result.
    pub(super) fn cmyk_compose_snapshot(
        &self,
        pixmap: &Pixmap,
        gs: &GraphicsState,
        doc: &PdfDocument,
        fill_side: bool,
    ) -> Option<Vec<u8>> {
        if self.cmyk_compose_active(gs, doc, fill_side) {
            Some(pixmap.data().to_vec())
        } else {
            None
        }
    }

    /// Snapshot the pixmap when the spot-lane mirror is about to fire.
    /// Returns `Some(pixmap_bytes)` when the sidecar is allocated AND
    /// the active side has at least one spot ink in the sidecar's
    /// discovered spot set; `None` otherwise. The mirror helper
    /// (`mirror_spot_paint_into_sidecar_with_coverage`) uses the
    /// snapshot to recover painted-pixel positions via a snapshot-vs-
    /// post-paint diff when the caller has no pre-rasterised coverage
    /// mask. Path-paint callers pass the pre-rasterised coverage
    /// directly and ignore the snapshot's diff role.
    pub(super) fn spot_paint_snapshot(
        &self,
        pixmap: &Pixmap,
        gs: &GraphicsState,
        fill_side: bool,
    ) -> Option<Vec<u8>> {
        if !self.spot_paint_active(gs, fill_side) {
            return None;
        }
        Some(pixmap.data().to_vec())
    }

    /// Fill-side spot snapshot for a text show, additionally gated on the
    /// fill-producing text render modes (`Tr` 0/2/4/6). ISO 32000-1 §9.3.6
    /// Table 106: modes 1/3/5/7 lay down no visible *fill* mark — mode 3 is
    /// fully invisible, 1/5 stroke only, 7 clip only. The spot mirror derives
    /// its coverage from [`Self::coverage_only_gs`], which force-overrides the
    /// render mode to 0 so the coverage scratch always paints; without this
    /// gate an invisible (`3 Tr`) or stroke-only show would still write the
    /// spot/InkA lane where nothing was painted. `spot_paint_active` cannot
    /// carry this check because it is shared with path paints, for which the
    /// text render mode is meaningless.
    pub(super) fn text_fill_spot_snapshot(
        &self,
        pixmap: &Pixmap,
        gs: &GraphicsState,
    ) -> Option<Vec<u8>> {
        if !matches!(gs.render_mode, 0 | 2 | 4 | 6) {
            return None;
        }
        self.spot_paint_snapshot(pixmap, gs, true)
    }

    /// Snapshot the pixmap when the CMYK sidecar plane is present and
    /// the paint side carries a CMYK colour. The plane mirror runs at
    /// every CMYK paint (opaque or transparent) so the sidecar stays
    /// in sync with the page's plate state. The mirror helper
    /// `mirror_cmyk_paint_into_sidecar` consumes the snapshot + post-
    /// paint pixmap to identify the painted region and writes updated
    /// CMYK quadruples at those pixels.
    pub(super) fn cmyk_sidecar_snapshot(
        &self,
        pixmap: &Pixmap,
        gs: &GraphicsState,
        fill_side: bool,
    ) -> Option<Vec<u8>> {
        self.cmyk_sidecar.as_ref()?;
        let has_cmyk = if fill_side {
            gs.fill_color_cmyk.is_some()
        } else {
            gs.stroke_color_cmyk.is_some()
        };
        if !has_cmyk {
            return None;
        }
        Some(pixmap.data().to_vec())
    }

    /// After a CMYK paint (opaque or transparent), write updated CMYK
    /// quadruples to the sidecar plane at painted pixels. The
    /// effective coverage is recovered from the snapshot vs post-paint
    /// pixmap diff so AA-edge pixels carry the correct partial CMYK.
    /// Skipped silently when the sidecar is None (detection-OFF) or
    /// when the painted-pixel-recovery cannot proceed (e.g. the
    /// rasteriser produced no observable diff).
    ///
    /// Called only when the paint is OPAQUE (no transparency
    /// composition needed). For transparent paints, the compose-first
    /// path is the source of truth for sidecar updates — it already
    /// mirrors the composed quadruple after compositing.
    ///
    /// For overprint paints, sidecar update happens inside
    /// [`Self::apply_overprint_after_paint`] which handles plate
    /// merging.
    pub(super) fn mirror_cmyk_paint_into_sidecar(
        &mut self,
        pixmap: &Pixmap,
        snapshot: &[u8],
        gs: &GraphicsState,
        doc: &PdfDocument,
        fill_side: bool,
    ) {
        let (sc, sm, sy, sk) = if fill_side {
            match gs.fill_color_cmyk {
                Some(v) => v,
                None => return,
            }
        } else {
            match gs.stroke_color_cmyk {
                Some(v) => v,
                None => return,
            }
        };

        // Skip when compose-first or overprint paths handle the
        // sidecar update themselves. Those paths run within their
        // own `apply_*_after_paint` helpers and write composed /
        // merged CMYK directly.
        let alpha = if fill_side {
            gs.fill_alpha
        } else {
            gs.stroke_alpha
        };
        let overprint = if fill_side {
            gs.fill_overprint
        } else {
            gs.stroke_overprint
        };
        let transparent = alpha < 1.0 || gs.blend_mode != "Normal" || gs.smask.is_some();
        if transparent || overprint {
            return;
        }

        // For opaque CMYK paints the post-paint RGB came through the
        // ICC convert-first (or additive-clamp fallback) path. To
        // detect painted pixels we look at the snapshot vs post-paint
        // diff; for AA-edge pixels we need to recover the effective
        // coverage so the sidecar carries the right partial-coverage
        // CMYK.
        let src_rgb_ic = {
            let c_u8 = (sc.clamp(0.0, 1.0) * 255.0).round() as u8;
            let m_u8 = (sm.clamp(0.0, 1.0) * 255.0).round() as u8;
            let y_u8 = (sy.clamp(0.0, 1.0) * 255.0).round() as u8;
            let k_u8 = (sk.clamp(0.0, 1.0) * 255.0).round() as u8;
            if let Some(profile) = doc.output_intent_cmyk_profile() {
                let intent = crate::color::RenderingIntent::from_pdf_name(&gs.rendering_intent);
                let transform = self.icc_transform_cache.get_or_build(&profile, intent);
                let rgb = transform.convert_cmyk_pixel(c_u8, m_u8, y_u8, k_u8);
                [
                    rgb[0] as f32 / 255.0,
                    rgb[1] as f32 / 255.0,
                    rgb[2] as f32 / 255.0,
                ]
            } else {
                let (r, g, b) = cmyk_to_rgb(sc, sm, sy, sk);
                [r, g, b]
            }
        };

        let post = pixmap.data();
        let plane = match self.cmyk_sidecar.as_mut() {
            Some(s) => s.cmyk_mut(),
            None => return,
        };
        debug_assert_eq!(post.len(), snapshot.len());
        debug_assert_eq!(post.len(), plane.len());

        for px in 0..(post.len() / 4) {
            let off = px * 4;
            let painted = post[off] != snapshot[off]
                || post[off + 1] != snapshot[off + 1]
                || post[off + 2] != snapshot[off + 2]
                || post[off + 3] != snapshot[off + 3];
            if !painted {
                continue;
            }

            // Recover effective coverage c from the source-over blend
            // on the channel with maximum |snap - src|.
            let snap_r = snapshot[off] as f32 / 255.0;
            let snap_g = snapshot[off + 1] as f32 / 255.0;
            let snap_b = snapshot[off + 2] as f32 / 255.0;
            let post_r = post[off] as f32 / 255.0;
            let post_g = post[off + 1] as f32 / 255.0;
            let post_b = post[off + 2] as f32 / 255.0;

            let diffs = [
                (snap_r - src_rgb_ic[0]).abs(),
                (snap_g - src_rgb_ic[1]).abs(),
                (snap_b - src_rgb_ic[2]).abs(),
            ];
            let (max_idx, max_diff) =
                diffs
                    .iter()
                    .enumerate()
                    .fold(
                        (0usize, 0.0_f32),
                        |acc, (i, &v)| if v > acc.1 { (i, v) } else { acc },
                    );
            let coverage = if max_diff > 1.0 / 255.0 {
                let (snap_ch, post_ch, src_ch) = match max_idx {
                    0 => (snap_r, post_r, src_rgb_ic[0]),
                    1 => (snap_g, post_g, src_rgb_ic[1]),
                    _ => (snap_b, post_b, src_rgb_ic[2]),
                };
                ((snap_ch - post_ch) / (snap_ch - src_ch)).clamp(0.0, 1.0)
            } else {
                1.0
            };

            // Sidecar backdrop CMYK.
            let dc = plane[off] as f32 / 255.0;
            let dm = plane[off + 1] as f32 / 255.0;
            let dy = plane[off + 2] as f32 / 255.0;
            let dk = plane[off + 3] as f32 / 255.0;

            // Source-over CMYK blend at effective coverage.
            let mc = coverage * sc + (1.0 - coverage) * dc;
            let mm = coverage * sm + (1.0 - coverage) * dm;
            let my = coverage * sy + (1.0 - coverage) * dy;
            let mk = coverage * sk + (1.0 - coverage) * dk;

            plane[off] = (mc.clamp(0.0, 1.0) * 255.0).round() as u8;
            plane[off + 1] = (mm.clamp(0.0, 1.0) * 255.0).round() as u8;
            plane[off + 2] = (my.clamp(0.0, 1.0) * 255.0).round() as u8;
            plane[off + 3] = (mk.clamp(0.0, 1.0) * 255.0).round() as u8;
        }
    }
}
