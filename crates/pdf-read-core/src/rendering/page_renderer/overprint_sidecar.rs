use super::*;

impl PageRenderer {
    /// Take a snapshot of `pixmap` when the graphics state has the
    /// overprint parameter active for the targeted side. Used by
    /// [`Self::apply_overprint_after_paint`] to recover the pre-paint
    /// pixel state in the painted region so the §11.7.4.3
    /// CompatibleOverprint blend function can be applied.
    ///
    /// The snapshot fires for every source colour space class
    /// classified by [`source_for_overprint`] — DeviceCMYK direct,
    /// DeviceGray/RGB/CIE/ICCBased process spaces, and
    /// Separation/DeviceN. The per-channel blend function dispatches
    /// on the source class; without the snapshot the painted region
    /// could not be identified for compositing.
    pub(super) fn overprint_snapshot(
        &self,
        pixmap: &Pixmap,
        gs: &GraphicsState,
        fill_side: bool,
    ) -> Option<Vec<u8>> {
        if source_for_overprint(gs, fill_side).is_some() {
            Some(pixmap.data().to_vec())
        } else {
            None
        }
    }

    /// Apply §11.7.4 composite overprint correction to the painted
    /// region. For each pixel where the paint contributed (snapshot
    /// differs from the post-paint pixmap), read the *snapshot's* RGB,
    /// invert to CMYK, and per-plate compose with the new paint's CMYK
    /// quadruple under the active OPM rule:
    ///
    ///   - OPM=0 (standard): non-source plates are knocked out to 0
    ///     except where overprint preserves them; for the composite
    ///     preview the simplest implementation honours "non-zero
    ///     source plate replaces dest" and "zero source plate is
    ///     transparent for that plate, dest preserved".
    ///   - OPM=1 (nonzero): zero source components are transparent for
    ///     their plate (dest preserved); non-zero replace dest plate.
    ///
    /// The merged CMYK is converted back to RGB and written to the
    /// destination pixel, replacing the naïve over-paint result.
    /// Coverage-aware overprint correction. Like
    /// [`Self::apply_cmyk_compose_after_paint_with_coverage`] but for
    /// the §11.7.4 plate merge. Reads backdrop CMYK from the sidecar
    /// instead of the additive-clamp inversion of the snapshot RGB.
    /// Falls back to [`Self::apply_overprint_after_paint`] when the
    /// sidecar is None.
    pub(super) fn apply_overprint_after_paint_with_coverage(
        &mut self,
        pixmap: &mut Pixmap,
        snapshot: &[u8],
        coverage: Option<&[u8]>,
        gs: &GraphicsState,
        doc: &PdfDocument,
        fill_side: bool,
    ) {
        if self.cmyk_sidecar.is_none() || coverage.is_none() {
            self.apply_overprint_after_paint(pixmap, snapshot, gs, doc, fill_side);
            return;
        }

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
        let coverage = coverage.expect("checked above");

        let icc_path = doc.output_intent_cmyk_profile().is_some();
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
        // Hoist the ICC transform once per call rather than once per pixel:
        // the cache key includes `profile.content_hash()` (a SipHash over
        // every byte of the profile blob), so a per-pixel lookup on a
        // full-page overprint fill ran tens of GB of hash work for the
        // same (profile, intent). The sibling diff-driven path hoists the
        // same way.
        let icc_transform = match (icc_profile.as_ref(), icc_intent) {
            (Some(profile), Some(intent)) => {
                Some(self.icc_transform_cache.get_or_build(profile, intent))
            }
            _ => None,
        };

        let dest = pixmap.data_mut();
        for px in 0..(dest.len() / 4) {
            let off = px * 4;
            let cov = coverage[px];
            if cov == 0 {
                continue;
            }
            // Effective alpha for this pixel — §11.3.3's α'.
            let c_alpha = ((cov as f32 / 255.0) * alpha_g).clamp(0.0, 1.0);

            // Backdrop CMYK from sidecar.
            let plane = self.cmyk_sidecar.as_ref().expect("checked above").cmyk();
            let dc = plane[off] as f32 / 255.0;
            let dm = plane[off + 1] as f32 / 255.0;
            let dy = plane[off + 2] as f32 / 255.0;
            let dk_existing = plane[off + 3] as f32 / 255.0;

            // §11.7.4.3 per-channel CompatibleOverprint composed with α.
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

            dest[off] = r_byte;
            dest[off + 1] = g_byte;
            dest[off + 2] = b_byte;

            // Mirror merged CMYK into sidecar.
            let plane = self.cmyk_sidecar.as_mut().expect("re-borrow").cmyk_mut();
            plane[off] = (mc.clamp(0.0, 1.0) * 255.0).round() as u8;
            plane[off + 1] = (mm.clamp(0.0, 1.0) * 255.0).round() as u8;
            plane[off + 2] = (my.clamp(0.0, 1.0) * 255.0).round() as u8;
            plane[off + 3] = (mk.clamp(0.0, 1.0) * 255.0).round() as u8;
        }
        let _ = snapshot;
    }

    /// Snapshot the pixmap when the sidecar is active AND the current
    /// paint is an RGB-source paint (DeviceRGB / DeviceGray / CalGray /
    /// RGB ICCBased — i.e. `fill_color_cmyk` is None on the active
    /// side). ISO 32000-1 §11.3.4 defines the §11.3.3 blend / composite
    /// computation that operates inside a single colour space; the
    /// "ONE blend space" mandate itself is §11.4.5.1's `/Group /CS`
    /// definition. On a CMYK OutputIntents page the group blend space
    /// IS CMYK (§11.4.5.1 default for a page-level transparency group
    /// derived from the document's OutputIntent), so an RGB-source
    /// paint must be converted to CMYK at paint-resolution time and
    /// mirrored into the sidecar. The companion helper
    /// [`Self::mirror_rgb_paint_into_sidecar`] runs the conversion +
    /// per-pixel composition.
    pub(super) fn cmyk_sidecar_snapshot_for_rgb_paint(
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
        if has_cmyk {
            // The CMYK mirror path handles this paint; the RGB mirror
            // must NOT double-touch the sidecar.
            return None;
        }
        Some(pixmap.data().to_vec())
    }

    /// Convert the active side's RGB colour to a CMYK quadruple using
    /// the document's OutputIntent CMYK profile when available, or the
    /// §10.3.5 inverse `(C, M, Y) = (1-R, 1-G, 1-B)` with `K = 0`
    /// fallback when the active backend has no CMYK output path. The
    /// fallback loses ink-coverage information in the K plane —
    /// documented behaviour, observable only when the destination
    /// press carries non-zero K under the converted RGB region.
    pub(super) fn resolve_rgb_paint_to_cmyk(
        &mut self,
        gs: &GraphicsState,
        doc: &PdfDocument,
        fill_side: bool,
    ) -> (f32, f32, f32, f32) {
        let (r, g, b) = if fill_side {
            gs.fill_color_rgb
        } else {
            gs.stroke_color_rgb
        };
        let r = r.clamp(0.0, 1.0);
        let g = g.clamp(0.0, 1.0);
        let b = b.clamp(0.0, 1.0);
        if let Some(profile) = doc.output_intent_cmyk_profile() {
            let intent = crate::color::RenderingIntent::from_pdf_name(&gs.rendering_intent);
            if let Some(transform) = self
                .icc_transform_cache
                .get_or_build_srgb_to_cmyk(&profile, intent)
            {
                let cmyk = transform.convert_pixel([r, g, b]);
                return (cmyk[0], cmyk[1], cmyk[2], cmyk[3]);
            }
        }
        // Process-ink separation for the qcms / no-CMM backends: the inverse of
        // the tetralinear `crate::color::cmyk_to_rgb`, so a pure-RGB paint
        // mirrored into the CMYK sidecar and composited back round-trips within
        // the process gamut (an out-of-gamut sRGB paint gamut-compresses). K
        // stays 0 (no black generation). Replaces the additive `(1-R,1-G,1-B)`.
        //
        // When the document catalog DECLARES an /OutputIntents array
        // but `output_intent_cmyk_profile()` returns `None`, the
        // producer asked for a press conversion that we couldn't honour
        // (e.g. profile bytes failed to parse, or no entry carried a
        // /N=4 /DestOutputProfile). Falling through to the K=0 inverse
        // silently degrades press output — the K plane goes empty
        // where the OutputIntent profile would have allocated black
        // ink. Log a one-shot warning so this is observable until
        // upstream issue yfedoseev/pdf_oxide#712 lands the proper
        // profile-parse-error diagnostic. When no /OutputIntents
        // declaration is present the K=0 fallback is the documented
        // device-RGB behaviour and stays silent.
        if doc.has_output_intents_declaration() && !self.k_zero_warning_emitted {
            log::warn!(
                "rgb→cmyk fallback fired with K=0 while document declares \
                 /OutputIntents. Profile lookup returned None (likely an \
                 unparseable /DestOutputProfile stream); press output \
                 will degrade in the K plane. Tracked upstream as \
                 yfedoseev/pdf_oxide#712."
            );
            self.k_zero_warning_emitted = true;
        }
        crate::color::rgb_to_cmyk(r, g, b)
    }

    /// Mirror an RGB-source paint into the CMYK sidecar via §11.3.4 +
    /// §11.4.5.1 blend-space conversion (§11.4.5.1 defines the group's
    /// /CS as the single blend colour space; §11.3.4 is the per-pixel
    /// compositing computation that runs inside it). Diff-driven
    /// variant for paints with no pre-rasterised coverage; the
    /// with-coverage variant is the hot path under transparency.
    pub(super) fn mirror_rgb_paint_into_sidecar(
        &mut self,
        pixmap: &Pixmap,
        snapshot: &[u8],
        gs: &GraphicsState,
        doc: &PdfDocument,
        fill_side: bool,
    ) {
        if self.cmyk_sidecar.is_none() {
            return;
        }
        let has_cmyk = if fill_side {
            gs.fill_color_cmyk.is_some()
        } else {
            gs.stroke_color_cmyk.is_some()
        };
        if has_cmyk {
            return;
        }
        // Skip overprint paints — overprint is meaningful only on
        // process-channel CMYK sources per §11.7.4.3 Table 149, and
        // the RGB source has no plate assignment to merge.
        let overprint = if fill_side {
            gs.fill_overprint
        } else {
            gs.stroke_overprint
        };
        if overprint {
            return;
        }

        let alpha = if fill_side {
            gs.fill_alpha
        } else {
            gs.stroke_alpha
        };
        let (sc, sm, sy, sk) = self.resolve_rgb_paint_to_cmyk(gs, doc, fill_side);

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
            // Effective coverage from the alpha-channel delta. For
            // opaque RGB paints the post-alpha is 255 against any
            // backdrop, so coverage = 1. For transparent paints we
            // bound via the alpha; the visible pixmap diff carries
            // alpha edge contributions, but for the §11.3.4 +
            // §11.4.5.1 sidecar mirror the conservative choice is to
            // mirror at the paint's nominal alpha — over-mirroring at
            // an AA-edge pixel still produces a smoothly-graded CMYK
            // backdrop and the next paint's coverage mask defines the
            // final composite.
            let eff = alpha.clamp(0.0, 1.0);
            let dc = plane[off] as f32 / 255.0;
            let dm = plane[off + 1] as f32 / 255.0;
            let dy = plane[off + 2] as f32 / 255.0;
            let dk = plane[off + 3] as f32 / 255.0;
            let mc = eff * sc + (1.0 - eff) * dc;
            let mm = eff * sm + (1.0 - eff) * dm;
            let my = eff * sy + (1.0 - eff) * dy;
            let mk = eff * sk + (1.0 - eff) * dk;
            plane[off] = (mc.clamp(0.0, 1.0) * 255.0).round() as u8;
            plane[off + 1] = (mm.clamp(0.0, 1.0) * 255.0).round() as u8;
            plane[off + 2] = (my.clamp(0.0, 1.0) * 255.0).round() as u8;
            plane[off + 3] = (mk.clamp(0.0, 1.0) * 255.0).round() as u8;
        }
    }

    /// Coverage-aware mirror of RGB-source paints into the CMYK
    /// sidecar. Pattern matches [`Self::mirror_cmyk_paint_into_sidecar_with_coverage`].
    pub(super) fn mirror_rgb_paint_into_sidecar_with_coverage(
        &mut self,
        pixmap: &Pixmap,
        snapshot: &[u8],
        coverage: Option<&[u8]>,
        gs: &GraphicsState,
        doc: &PdfDocument,
        fill_side: bool,
    ) {
        if self.cmyk_sidecar.is_none() || coverage.is_none() {
            self.mirror_rgb_paint_into_sidecar(pixmap, snapshot, gs, doc, fill_side);
            return;
        }
        let has_cmyk = if fill_side {
            gs.fill_color_cmyk.is_some()
        } else {
            gs.stroke_color_cmyk.is_some()
        };
        if has_cmyk {
            return;
        }
        let overprint = if fill_side {
            gs.fill_overprint
        } else {
            gs.stroke_overprint
        };
        if overprint {
            return;
        }
        let alpha = if fill_side {
            gs.fill_alpha
        } else {
            gs.stroke_alpha
        };
        let (sc, sm, sy, sk) = self.resolve_rgb_paint_to_cmyk(gs, doc, fill_side);

        let coverage = coverage.expect("checked above");
        let plane = self
            .cmyk_sidecar
            .as_mut()
            .expect("checked above")
            .cmyk_mut();
        for px in 0..(plane.len() / 4) {
            let cov = coverage[px];
            if cov == 0 {
                continue;
            }
            // Effective alpha at this pixel = path coverage · paint alpha.
            let eff = (cov as f32 / 255.0) * alpha.clamp(0.0, 1.0);
            let off = px * 4;
            let dc = plane[off] as f32 / 255.0;
            let dm = plane[off + 1] as f32 / 255.0;
            let dy = plane[off + 2] as f32 / 255.0;
            let dk = plane[off + 3] as f32 / 255.0;
            let mc = eff * sc + (1.0 - eff) * dc;
            let mm = eff * sm + (1.0 - eff) * dm;
            let my = eff * sy + (1.0 - eff) * dy;
            let mk = eff * sk + (1.0 - eff) * dk;
            plane[off] = (mc.clamp(0.0, 1.0) * 255.0).round() as u8;
            plane[off + 1] = (mm.clamp(0.0, 1.0) * 255.0).round() as u8;
            plane[off + 2] = (my.clamp(0.0, 1.0) * 255.0).round() as u8;
            plane[off + 3] = (mk.clamp(0.0, 1.0) * 255.0).round() as u8;
        }
        let _ = snapshot;
    }

    /// Coverage-aware mirror of opaque CMYK paints into the sidecar.
    /// Like [`Self::mirror_cmyk_paint_into_sidecar`] but uses the
    /// pre-rasterised coverage instead of the snap-vs-dest diff.
    pub(super) fn mirror_cmyk_paint_into_sidecar_with_coverage(
        &mut self,
        pixmap: &Pixmap,
        snapshot: &[u8],
        coverage: Option<&[u8]>,
        gs: &GraphicsState,
        doc: &PdfDocument,
        fill_side: bool,
    ) {
        if self.cmyk_sidecar.is_none() || coverage.is_none() {
            self.mirror_cmyk_paint_into_sidecar(pixmap, snapshot, gs, doc, fill_side);
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
        // Skip when the paint is transparent or overprint — those
        // paths handle the sidecar update themselves.
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

        let coverage = coverage.expect("checked above");
        let plane = self
            .cmyk_sidecar
            .as_mut()
            .expect("checked above")
            .cmyk_mut();
        for px in 0..(plane.len() / 4) {
            let cov = coverage[px];
            if cov == 0 {
                continue;
            }
            let cov_f = cov as f32 / 255.0;
            let off = px * 4;
            let dc = plane[off] as f32 / 255.0;
            let dm = plane[off + 1] as f32 / 255.0;
            let dy = plane[off + 2] as f32 / 255.0;
            let dk = plane[off + 3] as f32 / 255.0;
            let mc = cov_f * sc + (1.0 - cov_f) * dc;
            let mm = cov_f * sm + (1.0 - cov_f) * dm;
            let my = cov_f * sy + (1.0 - cov_f) * dy;
            let mk = cov_f * sk + (1.0 - cov_f) * dk;
            plane[off] = (mc.clamp(0.0, 1.0) * 255.0).round() as u8;
            plane[off + 1] = (mm.clamp(0.0, 1.0) * 255.0).round() as u8;
            plane[off + 2] = (my.clamp(0.0, 1.0) * 255.0).round() as u8;
            plane[off + 3] = (mk.clamp(0.0, 1.0) * 255.0).round() as u8;
        }
        let _ = snapshot;
        let _ = doc;
    }

    /// Predicate: should the spot-lane mirror fire for the current paint?
    ///
    /// Returns `true` when:
    /// 1. The sidecar is allocated (page declares transparency / overprint
    ///    AND a CMYK OutputIntent is present).
    /// 2. The active side declares spot inks via `gs.fill_spot_inks` /
    ///    `gs.stroke_spot_inks` (populated by SetFillColorN /
    ///    SetStrokeColorN when the colour space is /Separation or
    ///    /DeviceN per ISO 32000-1 §8.6.6.4 / §8.6.6.5).
    /// 3. At least one of those inks has a corresponding plane in
    ///    `sidecar.spot_names()`. An ink with no plane is the §8.6.6.3
    ///    "device has no plate for this colorant" branch — the
    ///    alternate colour space's CMYK decomposition lands on the
    ///    process plane via the existing CMYK mirror, so there is no
    ///    spot-lane work for this paint.
    pub(super) fn spot_paint_active(&self, gs: &GraphicsState, fill_side: bool) -> bool {
        let Some(sidecar) = self.cmyk_sidecar.as_ref() else {
            return false;
        };
        let inks = if fill_side {
            &gs.fill_spot_inks
        } else {
            &gs.stroke_spot_inks
        };
        if inks.is_empty() {
            return false;
        }
        inks.iter()
            .any(|(name, _)| sidecar.spot_index(name).is_some())
    }

    /// Apply per-pixel spot lane composition for the most recent paint.
    ///
    /// Composition follows ISO 32000-1 §11.3.3 (basic compositing
    /// formula) + §11.7.4.2 (per-lane BM dispatch). For each active
    /// source spot ink whose plane exists on the page:
    ///
    /// 1. Classify the requested `gs.blend_mode` via
    ///    [`BlendModeClass::from_name`]. The §11.6.3 unknown-name
    ///    fallback keeps unrecognised modes on the /Normal path.
    /// 2. Read the spot's per-lane dispatch
    ///    ([`BlendModeClass::spot_dispatch`]) — for
    ///    [`SpotBlendDispatch::SubstituteNormal`] the §11.7.4.2 rule
    ///    forces /Normal on the spot lane regardless of the requested
    ///    mode.
    /// 3. Compose the new tint per pixel:
    ///    `t_r = (1 - α') · t_b + α' · B(t_b, t_s)` where
    ///    `α' = coverage · gs_alpha`, `t_b` is the backdrop tint,
    ///    `t_s` is the source tint, and `B(·, ·)` is the dispatched
    ///    blend function on subtractive tints. Per §11.3.5.2 Table 136
    ///    the separable formulas operate on additive components — for
    ///    /Normal and the white-preserving modes the subtractive form
    ///    is mathematically equivalent (the formulas are component-wise
    ///    monotonic), so we apply them directly on tint values without
    ///    the additive↔subtractive conversion round-trip.
    ///
    /// Spot inks active on the source but with no plane in the sidecar
    /// (device does not carry the colorant per §8.6.6.3) are silently
    /// skipped — the composite RGB pixmap already received the
    /// alternate-CS approximation through the rasteriser.
    ///
    /// Other spot inks (in `sidecar.spot_names()` but NOT in the
    /// source's `gs.fill_spot_inks` / `gs.stroke_spot_inks`) are NOT
    /// touched. Per §11.7.3, every paint conceptually hits every
    /// component; for unsourced components the spec assigns "additive
    /// 1.0 / subtractive 0.0". Under /Normal: result = source 0.0
    /// composed against backdrop t_b gives `(1 - α') · t_b + α' · 0 =
    /// (1 - α') · t_b` — which for opaque paints `(α' = 1)` would
    /// ERASE the backdrop. Per §11.7.4.3 CompatibleOverprint, when
    /// overprint is enabled the spec instead preserves the backdrop on
    /// unsourced channels (B(c_b, c_s) = c_b). We adopt the
    /// overprint-preserving semantics unconditionally for unsourced
    /// spot lanes: real-world PDFs that target spot inks almost always
    /// expect "paint only what I said to paint" (the CompatibleOverprint
    /// behaviour), and the erase-on-unsourced policy under /Normal
    /// without overprint produces visually wrong output that no
    /// prepress workflow desires. This is pinned as
    /// [`HONEST_GAP_SPOT_LANE_UNSOURCED_PRESERVE_BACKDROP`] in the
    /// probes.
    pub(super) fn mirror_spot_paint_into_sidecar_with_coverage(
        &mut self,
        pixmap: &Pixmap,
        snapshot: &[u8],
        coverage: Option<&[u8]>,
        gs: &GraphicsState,
        fill_side: bool,
    ) {
        if !self.spot_paint_active(gs, fill_side) {
            return;
        }

        let source_inks: Vec<(String, f32)> = if fill_side {
            gs.fill_spot_inks.clone()
        } else {
            gs.stroke_spot_inks.clone()
        };
        let gs_alpha = if fill_side {
            gs.fill_alpha
        } else {
            gs.stroke_alpha
        };

        // §11.7.4.2 dispatch: classify the requested BM once.
        let class = crate::rendering::sidecar::BlendModeClass::from_name(&gs.blend_mode);
        // Per §11.7.4.2 the spot lane either uses the requested BM
        // unchanged, or substitutes /Normal. SubstituteNormal returns
        // "Normal" so the separable_blend helper takes the c_s path
        // identically.
        let effective_bm: &str = match class.spot_dispatch() {
            crate::rendering::sidecar::SpotBlendDispatch::UseRequested => gs.blend_mode.as_str(),
            crate::rendering::sidecar::SpotBlendDispatch::SubstituteNormal => "Normal",
        };

        // Build a coverage source. Two shapes:
        // * `coverage`: pre-rasterised path coverage from the path-paint
        //   helpers (`rasterise_fill_coverage` / `rasterise_stroke_coverage`).
        //   Bytes are 0..255 effective coverage per pixel.
        // * `None`: paint sites that don't have a separate rasteriser
        //   call (FillStroke combos, text, shading, Do). Fall back to a
        //   snapshot-vs-post diff: any pixel that changed is treated as
        //   "fully painted" (coverage = 255). This loses partial-coverage
        //   fidelity at AA edges; interior pixels are byte-exact.
        let post = pixmap.data();
        let computed_coverage: Vec<u8>;
        let cov_slice: &[u8] = if let Some(c) = coverage {
            c
        } else {
            debug_assert_eq!(post.len(), snapshot.len());
            computed_coverage = (0..post.len() / 4)
                .map(|px| {
                    let off = px * 4;
                    let changed = post[off] != snapshot[off]
                        || post[off + 1] != snapshot[off + 1]
                        || post[off + 2] != snapshot[off + 2]
                        || post[off + 3] != snapshot[off + 3];
                    if changed {
                        255
                    } else {
                        0
                    }
                })
                .collect();
            &computed_coverage
        };

        let sidecar = match self.cmyk_sidecar.as_mut() {
            Some(s) => s,
            None => return,
        };

        for (name, tint) in source_inks {
            // §8.6.6.3: ink not in the device's plate set → no spot
            // lane to write. The composite RGB pixmap already carries
            // the alternate-CS approximation.
            let Some(idx) = sidecar.spot_index(&name) else {
                continue;
            };
            let Some(plane) = sidecar.spot_plane_mut(idx) else {
                continue;
            };
            // The `tint` value is the operator's component for this
            // colorant — already subtractive per §8.6.6.4 / §8.6.6.5.
            let c_s = tint.clamp(0.0, 1.0);
            debug_assert_eq!(plane.len(), cov_slice.len());

            for (px, cov) in cov_slice.iter().enumerate() {
                let cov = *cov;
                if cov == 0 {
                    continue;
                }
                // Effective coverage·alpha — §11.3.3's α_s.
                let alpha = (cov as f32 / 255.0) * gs_alpha;
                let alpha = alpha.clamp(0.0, 1.0);
                let t_b = plane[px] as f32 / 255.0;
                let blended = crate::rendering::sidecar::separable_blend(effective_bm, t_b, c_s);
                let t_r = (1.0 - alpha) * t_b + alpha * blended;
                plane[px] = (t_r.clamp(0.0, 1.0) * 255.0).round() as u8;
            }
        }
    }
}
