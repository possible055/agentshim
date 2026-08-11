use super::*;

impl PageRenderer {
    /// Coverage-aware compose-first that takes a pre-rasterised path
    /// coverage mask. Used when the CMYK sidecar is allocated so the
    /// "painted region" is identified independent of the snap-vs-dest
    /// diff (which fails when source and backdrop ICC-RGB collide,
    /// producing painted=false at pixels that the path actually
    /// covered). Falls through to the standard
    /// [`Self::apply_cmyk_compose_after_paint`] when the sidecar is
    /// None.
    pub(super) fn apply_cmyk_compose_after_paint_with_coverage(
        &mut self,
        pixmap: &mut Pixmap,
        snapshot: &[u8],
        coverage: Option<&[u8]>,
        gs: &GraphicsState,
        doc: &PdfDocument,
        fill_side: bool,
    ) {
        if self.cmyk_sidecar.is_none() || coverage.is_none() {
            // Fall back to the diff-driven path. Detection-OFF
            // byte-identical behaviour.
            self.apply_cmyk_compose_after_paint(pixmap, snapshot, gs, doc, fill_side);
            return;
        }

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
        let alpha_g = if fill_side {
            gs.fill_alpha
        } else {
            gs.stroke_alpha
        };
        let profile = match doc.output_intent_cmyk_profile() {
            Some(p) => p,
            None => return,
        };
        let intent = crate::color::RenderingIntent::from_pdf_name(&gs.rendering_intent);
        let coverage = coverage.expect("checked above");
        // Hoist the ICC transform out of the per-pixel loop. The cache key
        // includes `profile.content_hash()`, which hashes every byte of the
        // ICC profile blob — a per-pixel lookup on a full-page transparency
        // fill ran tens of GB of hash work for the same (profile, intent)
        // tuple every paint. The sibling diff-driven path
        // (`apply_cmyk_compose_after_paint`) hoists the same way.
        let transform = self.icc_transform_cache.get_or_build(&profile, intent);
        let dest = pixmap.data_mut();

        for px in 0..(dest.len() / 4) {
            let off = px * 4;
            let cov = coverage[px];
            if cov == 0 {
                continue;
            }
            let coverage_frac = cov as f32 / 255.0;
            let c_alpha = (coverage_frac * alpha_g).clamp(0.0, 1.0);

            // Backdrop CMYK from sidecar.
            let plane = self.cmyk_sidecar.as_ref().expect("checked above").cmyk();
            let dc = plane[off] as f32 / 255.0;
            let dm = plane[off + 1] as f32 / 255.0;
            let dy = plane[off + 2] as f32 / 255.0;
            let dk = plane[off + 3] as f32 / 255.0;

            let mc = c_alpha * sc + (1.0 - c_alpha) * dc;
            let mm = c_alpha * sm + (1.0 - c_alpha) * dm;
            let my = c_alpha * sy + (1.0 - c_alpha) * dy;
            let mk = c_alpha * sk + (1.0 - c_alpha) * dk;

            let mc_u8 = (mc.clamp(0.0, 1.0) * 255.0).round() as u8;
            let mm_u8 = (mm.clamp(0.0, 1.0) * 255.0).round() as u8;
            let my_u8 = (my.clamp(0.0, 1.0) * 255.0).round() as u8;
            let mk_u8 = (mk.clamp(0.0, 1.0) * 255.0).round() as u8;

            let rgb = transform.convert_cmyk_pixel(mc_u8, mm_u8, my_u8, mk_u8);

            dest[off] = rgb[0];
            dest[off + 1] = rgb[1];
            dest[off + 2] = rgb[2];

            // Mirror composed CMYK back to sidecar.
            let plane = self.cmyk_sidecar.as_mut().expect("re-borrow").cmyk_mut();
            plane[off] = mc_u8;
            plane[off + 1] = mm_u8;
            plane[off + 2] = my_u8;
            plane[off + 3] = mk_u8;
        }
        let _ = snapshot; // diff-path no longer consults the snapshot
    }

    pub(super) fn apply_cmyk_compose_after_paint(
        &mut self,
        pixmap: &mut Pixmap,
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
        let alpha_g = if fill_side {
            gs.fill_alpha
        } else {
            gs.stroke_alpha
        };
        let profile = match doc.output_intent_cmyk_profile() {
            Some(p) => p,
            None => return,
        };

        // Build a single ICC transform for this call. The renderer's
        // per-page IccTransformCache holds the compiled qcms transform
        // across the many paint operators on the page; we look it up
        // ONCE here and reuse the Arc<Transform> for every pixel in the
        // loop below. The cache key includes `profile.content_hash()`,
        // which hashes every byte of the profile blob (SipHash over
        // hundreds of KB on a typical CMYK profile); a per-pixel lookup
        // would re-hash the same blob on every paint.
        let intent = crate::color::RenderingIntent::from_pdf_name(&gs.rendering_intent);
        let transform = self.icc_transform_cache.get_or_build(&profile, intent);

        // Compute the convert-first source RGB the rasteriser actually
        // wrote into the pixmap. We need this to recover the effective
        // coverage `c·α` from the post-paint pixel:
        //   post = (c·α)·src_rgb_ic + (1 - c·α)·snap_rgb
        // The recovery picks the channel with maximum |snap - src| for
        // numerical stability and skips channels where the difference
        // is below a threshold.
        let src_rgb_ic = {
            let c_u8 = (sc.clamp(0.0, 1.0) * 255.0).round() as u8;
            let m_u8 = (sm.clamp(0.0, 1.0) * 255.0).round() as u8;
            let y_u8 = (sy.clamp(0.0, 1.0) * 255.0).round() as u8;
            let k_u8 = (sk.clamp(0.0, 1.0) * 255.0).round() as u8;
            let rgb = transform.convert_cmyk_pixel(c_u8, m_u8, y_u8, k_u8);
            [
                rgb[0] as f32 / 255.0,
                rgb[1] as f32 / 255.0,
                rgb[2] as f32 / 255.0,
            ]
        };

        let dest = pixmap.data_mut();
        debug_assert_eq!(dest.len(), snapshot.len());

        for px in 0..(dest.len() / 4) {
            let off = px * 4;

            // Detect "this pixel was painted": any RGBA byte differs
            // between snapshot and current pixmap.
            let painted = dest[off] != snapshot[off]
                || dest[off + 1] != snapshot[off + 1]
                || dest[off + 2] != snapshot[off + 2]
                || dest[off + 3] != snapshot[off + 3];
            if !painted {
                continue;
            }

            let snap_r = snapshot[off] as f32 / 255.0;
            let snap_g = snapshot[off + 1] as f32 / 255.0;
            let snap_b = snapshot[off + 2] as f32 / 255.0;
            let post_r = dest[off] as f32 / 255.0;
            let post_g = dest[off + 1] as f32 / 255.0;
            let post_b = dest[off + 2] as f32 / 255.0;

            // Recover effective coverage c·α by inverting the source-
            // over alpha-blend on the channel with maximum |snap -
            // src_rgb_ic| (most numerically stable). Default to the
            // graphics-state alpha when the source RGB matches the
            // snapshot exactly on every channel — in that case the
            // pixel's RGB contribution is zero so any coverage value
            // produces the same result.
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

            let c_alpha = if max_diff > 1.0 / 255.0 {
                let (snap_ch, post_ch, src_ch) = match max_idx {
                    0 => (snap_r, post_r, src_rgb_ic[0]),
                    1 => (snap_g, post_g, src_rgb_ic[1]),
                    _ => (snap_b, post_b, src_rgb_ic[2]),
                };
                ((snap_ch - post_ch) / (snap_ch - src_ch)).clamp(0.0, 1.0)
            } else {
                // Source RGB ≈ snapshot RGB — coverage is moot, but use
                // the graphics-state alpha as a sensible fallback so a
                // non-Normal blend mode still gets the right magnitude.
                alpha_g
            };

            // Backdrop CMYK source. Two paths:
            //
            //  (a) Sidecar plane present — read CMYK quadruple directly
            //      from the page-resident plate buffer. This is the
            //      press-accurate path; under a non-linear ICC the
            //      additive-clamp inversion below is lossy.
            //  (b) No sidecar — fall back to §10.3.5 additive-clamp
            //      inversion of the snapshot RGB. Exact for the
            //      baseline-white backdrop and the additive-clamp
            //      fallback OutputIntent path; bounded-loss when the
            //      backdrop went through a non-linear ICC. Documented
            //      gap, kept for the detection-OFF path.
            let (dc, dm, dy, dk) =
                if let Some(plane) = self.cmyk_sidecar.as_ref().map(CmykSidecar::cmyk) {
                    (
                        plane[off] as f32 / 255.0,
                        plane[off + 1] as f32 / 255.0,
                        plane[off + 2] as f32 / 255.0,
                        plane[off + 3] as f32 / 255.0,
                    )
                } else {
                    (
                        (1.0 - snap_r).max(0.0),
                        (1.0 - snap_g).max(0.0),
                        (1.0 - snap_b).max(0.0),
                        0.0_f32,
                    )
                };

            // Compose in CMYK source space at effective coverage·alpha.
            let mc = c_alpha * sc + (1.0 - c_alpha) * dc;
            let mm = c_alpha * sm + (1.0 - c_alpha) * dm;
            let my = c_alpha * sy + (1.0 - c_alpha) * dy;
            let mk = c_alpha * sk + (1.0 - c_alpha) * dk;

            // Convert the composed CMYK through the OutputIntent ICC,
            // reusing the loop-hoisted `transform`.
            let mc_u8 = (mc.clamp(0.0, 1.0) * 255.0).round() as u8;
            let mm_u8 = (mm.clamp(0.0, 1.0) * 255.0).round() as u8;
            let my_u8 = (my.clamp(0.0, 1.0) * 255.0).round() as u8;
            let mk_u8 = (mk.clamp(0.0, 1.0) * 255.0).round() as u8;
            let rgb = transform.convert_cmyk_pixel(mc_u8, mm_u8, my_u8, mk_u8);

            dest[off] = rgb[0];
            dest[off + 1] = rgb[1];
            dest[off + 2] = rgb[2];
            // Alpha unchanged — the source-over alpha rule is identical
            // in convert-first vs compose-first, so the tiny_skia
            // rasteriser's alpha output is correct as-is.

            // Mirror the composed CMYK into the sidecar so subsequent
            // paints see the press-accurate backdrop. The mirror is
            // bypassed when the sidecar is None (detection-OFF
            // byte-identical path).
            if let Some(plane) = self.cmyk_sidecar.as_mut().map(CmykSidecar::cmyk_mut) {
                plane[off] = mc_u8;
                plane[off + 1] = mm_u8;
                plane[off + 2] = my_u8;
                plane[off + 3] = mk_u8;
            }
        }
    }
}
