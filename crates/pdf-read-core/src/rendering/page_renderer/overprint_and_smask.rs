use super::*;

impl PageRenderer {
    /// Apply ISO 32000-1 §11.7.4.3 CompatibleOverprint to every painted
    /// pixel.
    ///
    /// The §11.7.4.3 blend function `B(c_b, c_s)` returns a subtractive
    /// tint per Table 149, dispatched on source colour space × OP × OPM:
    ///
    /// |Source CS                            |Component          |OP=true OPM=0|OP=true OPM=1                |
    /// |-------------------------------------|-------------------|-------------|-----------------------------|
    /// |DeviceCMYK direct                    |C, M, Y, K         |c_s          |c_s if c_s≠0 else c_b        |
    /// |DeviceCMYK direct                    |Process not in CMYK|c_s          |c_s                          |
    /// |DeviceCMYK direct                    |Spot               |c_b          |c_b                          |
    /// |Any other process CS (e.g. DeviceGray|Process            |c_s          |c_s                          |
    /// |  DeviceRGB, ICCBased, DeviceCMYK    |Spot               |c_b          |c_b                          |
    /// |  via sampled image)                 |                   |             |                             |
    /// |Separation / DeviceN                 |Process            |c_b          |c_b                          |
    /// |                                     |Named spot         |c_s          |c_s                          |
    /// |                                     |Unnamed spot       |c_b          |c_b                          |
    ///
    /// The OPM=1 zero-source-preserve rule is specific to row 1
    /// (DeviceCMYK directly specified). §11.7.4.5 makes this explicit:
    /// "Nonzero overprint mode shall apply only to painting operations
    /// that use the current colour in the graphics state when the
    /// current colour space is DeviceCMYK".
    ///
    /// Each painted pixel composes per §11.3.3 as
    /// `c_r = α · B(c_b, c_s) + (1 − α) · c_b`, where α is the effective
    /// shape×opacity at the pixel. This helper recovers α from the
    /// snapshot-vs-post-paint diff like the coverage-less compose path
    /// does; the coverage-aware variant
    /// ([`Self::apply_overprint_after_paint_with_coverage`]) reads α
    /// directly from the path coverage mask + `gs` alpha.
    ///
    /// The process lanes (CMYK) are written to the sidecar plane and
    /// converted to RGB via the OutputIntent ICC (falling back to the
    /// additive-clamp `cmyk_to_rgb` round-trip when no profile is
    /// available). Spot lanes are handled separately by
    /// [`Self::mirror_spot_paint_into_sidecar_with_coverage`] — for
    /// Separation / DeviceN sources the named spot lane carries c_s; for
    /// all other source classes the spot lane is preserved (no write),
    /// matching Table 149's spot row.
    pub(super) fn apply_overprint_after_paint(
        &mut self,
        pixmap: &mut Pixmap,
        snapshot: &[u8],
        gs: &GraphicsState,
        doc: &PdfDocument,
        fill_side: bool,
    ) {
        let Some(source) = source_for_overprint(gs, fill_side) else {
            return;
        };
        let opm = gs.overprint_mode;
        let alpha_g = if fill_side {
            gs.fill_alpha
        } else {
            gs.stroke_alpha
        };
        let (sc, sm, sy, sk) = source.cmyk;
        // ICC path active when the CMYK sidecar plane is present AND an
        // OutputIntent CMYK profile is available. The merged CMYK then
        // runs through the ICC; otherwise the additive-clamp
        // `cmyk_to_rgb` round-trip stays in place.
        let icc_path = self.cmyk_sidecar.is_some() && doc.output_intent_cmyk_profile().is_some();
        let icc_profile = if icc_path {
            doc.output_intent_cmyk_profile()
        } else {
            None
        };
        let icc_intent = if icc_path {
            Some(crate::color::RenderingIntent::from_pdf_name(
                &gs.rendering_intent,
            ))
        } else {
            None
        };
        // Hoist the ICC transform out of the per-pixel loop. The cache
        // key includes `profile.content_hash()` (SipHash over every
        // byte of the ICC profile blob); a per-pixel lookup re-hashed
        // hundreds of KB on every painted pixel.
        let icc_transform = match (icc_profile.as_ref(), icc_intent) {
            (Some(profile), Some(intent)) => {
                Some(self.icc_transform_cache.get_or_build(profile, intent))
            }
            _ => None,
        };

        // Pre-compute the convert-first source RGB the rasteriser
        // actually wrote. Used to invert the source-over alpha blend
        // and recover effective coverage·alpha per pixel. Mirrors the
        // `apply_cmyk_compose_after_paint` recovery for byte-identity
        // with the compose-first path.
        let src_rgb_ic = if let Some(transform) = icc_transform.as_ref() {
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
        } else {
            let (r, g, b) = cmyk_to_rgb(sc, sm, sy, sk);
            [r, g, b]
        };

        let dest = pixmap.data_mut();
        debug_assert_eq!(dest.len(), snapshot.len());

        for px in 0..(dest.len() / 4) {
            let off = px * 4;

            // Detect "this pixel was painted": any RGBA byte differs
            // between snapshot and current pixmap. Coverage-aware AA
            // pixels are detected too.
            let painted = dest[off] != snapshot[off]
                || dest[off + 1] != snapshot[off + 1]
                || dest[off + 2] != snapshot[off + 2]
                || dest[off + 3] != snapshot[off + 3];
            if !painted {
                continue;
            }

            // Recover effective coverage·alpha from the source-over
            // alpha blend on the most-stable channel — same shape as
            // apply_cmyk_compose_after_paint.
            let snap_r = snapshot[off] as f32 / 255.0;
            let snap_g = snapshot[off + 1] as f32 / 255.0;
            let snap_b = snapshot[off + 2] as f32 / 255.0;
            let post_r = dest[off] as f32 / 255.0;
            let post_g = dest[off + 1] as f32 / 255.0;
            let post_b = dest[off + 2] as f32 / 255.0;
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
                // Source RGB ≈ snapshot RGB — coverage is moot. Use the
                // graphics-state alpha as a sensible fallback.
                alpha_g
            };

            // Backdrop CMYK from sidecar; additive-clamp fallback when
            // the sidecar is None.
            let (dc, dm, dy, dk_existing) =
                if let Some(plane) = self.cmyk_sidecar.as_ref().map(CmykSidecar::cmyk) {
                    (
                        plane[off] as f32 / 255.0,
                        plane[off + 1] as f32 / 255.0,
                        plane[off + 2] as f32 / 255.0,
                        plane[off + 3] as f32 / 255.0,
                    )
                } else {
                    let dr = snapshot[off] as f32 / 255.0;
                    let dg = snapshot[off + 1] as f32 / 255.0;
                    let db = snapshot[off + 2] as f32 / 255.0;
                    (
                        (1.0 - dr).max(0.0),
                        (1.0 - dg).max(0.0),
                        (1.0 - db).max(0.0),
                        0.0_f32,
                    )
                };

            // Per-channel §11.7.4.3 CompatibleOverprint blend function,
            // then §11.3.3 composition with effective alpha.
            let mc =
                compose_overprint_channel(source.class, ProcessChannel::C, sc, dc, opm, c_alpha);
            let mm =
                compose_overprint_channel(source.class, ProcessChannel::M, sm, dm, opm, c_alpha);
            let my =
                compose_overprint_channel(source.class, ProcessChannel::Y, sy, dy, opm, c_alpha);
            let mk = compose_overprint_channel(
                source.class,
                ProcessChannel::K,
                sk,
                dk_existing,
                opm,
                c_alpha,
            );

            // CMYK → RGB conversion. ICC path for the press-accurate
            // case; additive-clamp `cmyk_to_rgb` for the fallback.
            let (r_byte, g_byte, b_byte) = if let Some(transform) = icc_transform.as_ref() {
                let mc_u8 = (mc.clamp(0.0, 1.0) * 255.0).round() as u8;
                let mm_u8 = (mm.clamp(0.0, 1.0) * 255.0).round() as u8;
                let my_u8 = (my.clamp(0.0, 1.0) * 255.0).round() as u8;
                let mk_u8 = (mk.clamp(0.0, 1.0) * 255.0).round() as u8;
                let rgb = transform.convert_cmyk_pixel(mc_u8, mm_u8, my_u8, mk_u8);
                (rgb[0], rgb[1], rgb[2])
            } else {
                let (rr, rg, rb) = cmyk_to_rgb(mc, mm, my, mk);
                (
                    (rr * 255.0).round().clamp(0.0, 255.0) as u8,
                    (rg * 255.0).round().clamp(0.0, 255.0) as u8,
                    (rb * 255.0).round().clamp(0.0, 255.0) as u8,
                )
            };

            // Preserve the painted pixel's alpha (post-paint alpha
            // already accounts for the paint's contribution); just
            // overwrite RGB with the per-channel composed value.
            dest[off] = r_byte;
            dest[off + 1] = g_byte;
            dest[off + 2] = b_byte;
            // Alpha unchanged.

            // Mirror the composed CMYK into the sidecar so subsequent
            // paints see the post-overprint backdrop.
            if let Some(plane) = self.cmyk_sidecar.as_mut().map(CmykSidecar::cmyk_mut) {
                plane[off] = (mc.clamp(0.0, 1.0) * 255.0).round() as u8;
                plane[off + 1] = (mm.clamp(0.0, 1.0) * 255.0).round() as u8;
                plane[off + 2] = (my.clamp(0.0, 1.0) * 255.0).round() as u8;
                plane[off + 3] = (mk.clamp(0.0, 1.0) * 255.0).round() as u8;
            }
        }
    }

    /// Modulate the destination pixmap's painted contribution by the
    /// soft mask declared on `gs`. The mask is rendered once per call
    /// from the referenced Form XObject; on rendering failure the
    /// snapshot is restored (the paint is suppressed entirely — safer
    /// than leaving the unmodulated paint, which would mis-render
    /// content the author intended to hide).
    ///
    /// Per ISO 32000-1:2008 §11.4.7, for each pixel:
    ///
    /// - `S=Alpha`: `mask_value = form_pixmap.alpha[px]`
    /// - `S=Luminosity`: `mask_value = 0.30 R + 0.59 G + 0.11 B` of form_pixmap
    ///
    /// Optional `/TR` transfer is evaluated on the mask value before
    /// modulation. The destination pixel is updated as a linear blend
    /// between `snapshot` and `pixmap` weighted by the mask:
    /// `dest = mask * pixmap + (1 - mask) * snapshot`.
    pub(super) fn apply_smask_after_paint(
        &mut self,
        pixmap: &mut Pixmap,
        snapshot: &[u8],
        spot_snapshot: Option<&[u8]>,
        gs: &GraphicsState,
        doc: &PdfDocument,
        page_num: usize,
        resources: &Object,
        base_transform: Transform,
    ) -> Result<()> {
        let smask = match gs.smask.as_ref() {
            Some(s) => s.clone(),
            None => return Ok(()),
        };

        // Defend against adversarial cyclic /SMask /G chains: the form
        // referenced by /G can itself declare /SMask on its own
        // content, re-entering this materialisation path. Without a
        // cap recursion is unbounded. At the cap the paint is left
        // unmodulated (the pre-paint snapshot is NOT restored — the
        // caller's paint stays visible) and the recursion unwinds.
        if self.smask_depth >= MAX_SMASK_DEPTH {
            log::warn!(
                "SMask materialisation reached MAX_SMASK_DEPTH={}; \
                 likely cyclic /SMask /G chain. Skipping further \
                 modulation on this paint.",
                MAX_SMASK_DEPTH
            );
            return Ok(());
        }
        self.smask_depth += 1;
        let result = self.apply_smask_after_paint_inner(
            pixmap,
            snapshot,
            spot_snapshot,
            &smask,
            doc,
            page_num,
            resources,
            base_transform,
        );
        self.smask_depth -= 1;
        result
    }

    pub(super) fn apply_smask_after_paint_inner(
        &mut self,
        pixmap: &mut Pixmap,
        snapshot: &[u8],
        spot_snapshot: Option<&[u8]>,
        smask: &crate::content::graphics_state::SoftMaskForm,
        doc: &PdfDocument,
        page_num: usize,
        resources: &Object,
        base_transform: Transform,
    ) -> Result<()> {
        // Render the Form XObject into a fresh pixmap. The pixmap
        // starts fully transparent for /S /Alpha (the spec default
        // backdrop is the black point, which projects to alpha=0).
        // For /S /Luminosity the optional /BC backdrop pre-fills with
        // the declared colour; absent /BC the spec default is the
        // colour space's black point (also fills with zeros).
        let w = pixmap.width();
        let h = pixmap.height();
        let mut mask_pixmap = match Pixmap::new(w, h) {
            Some(p) => p,
            None => {
                // Allocation failed — restore the snapshot to avoid
                // emitting an unmasked paint.
                pixmap.data_mut().copy_from_slice(snapshot);
                return Ok(());
            }
        };

        // Resolve the Form XObject. We load it before the /BC pre-fill
        // so the pre-fill can consult the Form's /Group /CS for
        // 5+ component DeviceN backdrops (the n=1/3/4 device-family
        // cases don't need the Group CS — array length disambiguates).
        let form_obj = match doc.load_object(smask.form_ref) {
            Ok(o) => o,
            Err(_) => {
                pixmap.data_mut().copy_from_slice(snapshot);
                return Ok(());
            }
        };

        let (form_dict, form_data) = match &form_obj {
            Object::Stream { dict, .. } => {
                // Decode through the encryption layer if present, the
                // same way render_form_xobject does at the main
                // dispatch site (page_renderer:2320).
                let data = doc.decode_stream_with_encryption(&form_obj, smask.form_ref)?;
                (dict.clone(), data)
            }
            _ => {
                pixmap.data_mut().copy_from_slice(snapshot);
                return Ok(());
            }
        };

        // For /S /Luminosity, pre-fill with the /BC backdrop if
        // present. The backdrop is in the Group colour space:
        //  - n=1   → /DeviceGray
        //  - n=3   → /DeviceRGB
        //  - n=4   → /DeviceCMYK
        //  - n>=5  → /DeviceN (or /NChannel) declared on the Form's
        //           /Group /CS. Evaluating an /DeviceN backdrop
        //           requires walking the Group /CS tint transform
        //           and projecting the alternate-space colour through
        //           the same path the renderer uses for /Separation /
        //           /DeviceN paints. The helper below handles that.
        if smask.subtype == crate::content::graphics_state::SoftMaskSubtype::Luminosity {
            if let Some(ref bc) = smask.backdrop {
                let (r, g, b) = match bc.len() {
                    1 => {
                        let v = (bc[0].clamp(0.0, 1.0) * 255.0).round() as u8;
                        (v, v, v)
                    }
                    3 => (
                        (bc[0].clamp(0.0, 1.0) * 255.0).round() as u8,
                        (bc[1].clamp(0.0, 1.0) * 255.0).round() as u8,
                        (bc[2].clamp(0.0, 1.0) * 255.0).round() as u8,
                    ),
                    4 => {
                        let (rf, gf, bf) = cmyk_to_rgb(bc[0], bc[1], bc[2], bc[3]);
                        (
                            (rf * 255.0).round() as u8,
                            (gf * 255.0).round() as u8,
                            (bf * 255.0).round() as u8,
                        )
                    }
                    n if n >= 5 => {
                        // §11.6.5.2 Table 144 + §8.6.6.5: when the
                        // Form group declares DeviceN / NChannel as
                        // its /CS, /BC carries n tints. Evaluate the
                        // group's tint transform on the BC tints and
                        // project the resulting alternate-space colour
                        // to RGB. Falls to (0, 0, 0) (the spec's
                        // black-point default) if the group's CS is
                        // not a recognised DeviceN.
                        evaluate_devicen_bc_to_rgb(&form_dict, bc, doc).unwrap_or((0, 0, 0))
                    }
                    _ => (0, 0, 0),
                };
                let data = mask_pixmap.data_mut();
                for px in 0..(w * h) as usize {
                    let off = px * 4;
                    data[off] = r;
                    data[off + 1] = g;
                    data[off + 2] = b;
                    data[off + 3] = 255;
                }
            }
        }

        let form_resources_obj = form_dict
            .get("Resources")
            .and_then(|r| doc.resolve_object(r).ok())
            .unwrap_or_else(|| resources.clone());

        // Render the form using the page's base transform: §11.6.5.2
        // mandates the mask be evaluated in the device space in effect
        // at the host paint, which carries both the DPI scale and the
        // PDF→device y-flip. Using `Transform::identity()` here would
        // leave the mask at PDF user-space (72 dpi, y-up) — mis-scaled
        // and y-flipped relative to the pixmap whenever DPI ≠ 72.
        // The form's /Matrix is still composed on top of `base_transform`
        // by `render_form_xobject`, so the mask remains positioned by
        // its own matrix within the page-aligned device frame.
        let _ = self.render_form_xobject(
            &mut mask_pixmap,
            &form_dict,
            &form_data,
            base_transform,
            doc,
            page_num,
            &form_resources_obj,
        );

        // Resolve /TR transfer function once. The audit fixture uses
        // a Type-2 power function (`N=2` squares the input); the
        // helper below covers Type 2 and falls through to identity
        // for unsupported types. PDF spec §11.4.7 requires identity
        // as the default when /TR is absent.
        let transfer = smask
            .transfer
            .as_ref()
            .and_then(|tr_obj| doc.resolve_object(tr_obj).ok())
            .and_then(|resolved| parse_transfer_function(doc, &resolved));

        // Apply the mask: pixmap = mask * pixmap + (1 - mask) * snapshot.
        let mask_data = mask_pixmap.data();
        let dest = pixmap.data_mut();
        debug_assert_eq!(mask_data.len(), dest.len());
        debug_assert_eq!(snapshot.len(), dest.len());

        // §11.3.3 + §11.7.3: the SMask alpha is a single shape/opacity
        // value per pixel that applies to BOTH process and spot colour
        // components. Compute the per-pixel mask alpha once, then
        // attenuate the visible pixmap (RGB+α) AND, when the sidecar
        // is allocated, every spot lane against its pre-mirror
        // snapshot.
        let pixel_count = dest.len() / 4;
        let mut mask_alpha: Vec<f32> = Vec::with_capacity(pixel_count);
        for px in 0..pixel_count {
            let off = px * 4;
            let mut m = match smask.subtype {
                crate::content::graphics_state::SoftMaskSubtype::Alpha => {
                    mask_data[off + 3] as f32 / 255.0
                }
                crate::content::graphics_state::SoftMaskSubtype::Luminosity => {
                    let r = mask_data[off] as f32 / 255.0;
                    let g = mask_data[off + 1] as f32 / 255.0;
                    let b = mask_data[off + 2] as f32 / 255.0;
                    0.30 * r + 0.59 * g + 0.11 * b
                }
            };

            if let Some(ref tf) = transfer {
                m = tf.eval(m).clamp(0.0, 1.0);
            }
            mask_alpha.push(m);

            let inv_m = 1.0 - m;
            for c in 0..4 {
                let painted = dest[off + c] as f32;
                let backed = snapshot[off + c] as f32;
                let blended = m * painted + inv_m * backed;
                dest[off + c] = blended.clamp(0.0, 255.0).round() as u8;
            }
        }

        // Spot lanes: apply the same SMask alpha attenuation to every
        // spot plane against its pre-mirror snapshot. Per §11.7.3, the
        // soft mask's alpha modulates the spot lane the same way it
        // modulates process channels — a single (shape, opacity) per
        // pixel applies to every lane class. Skipping this step (or
        // applying the SMask only to the pixmap) leaves the spot lanes
        // composed at α=1 while the visible pixmap is attenuated, so
        // the press plate output would over-deposit ink relative to
        // the visible composite by exactly the SMask attenuation
        // factor.
        if let (Some(pre_spots), Some(sidecar)) = (spot_snapshot, self.cmyk_sidecar.as_mut()) {
            let spots = sidecar.spots_all_mut();
            // The snapshot length tracks the page's spot plane count.
            // If the sidecar's plane count changed mid-paint (it
            // doesn't — fixed at page setup) the comparison would be
            // unsafe; debug-assert it stays in sync.
            debug_assert_eq!(spots.len(), pre_spots.len());
            let plane_size = pixel_count;
            let plane_count = spots.len() / plane_size;
            for plane_idx in 0..plane_count {
                let base = plane_idx * plane_size;
                for px in 0..plane_size {
                    let m = mask_alpha[px];
                    let inv_m = 1.0 - m;
                    let post = spots[base + px] as f32;
                    let pre = pre_spots[base + px] as f32;
                    let blended = m * post + inv_m * pre;
                    spots[base + px] = blended.clamp(0.0, 255.0).round() as u8;
                }
            }
        }

        Ok(())
    }
}
