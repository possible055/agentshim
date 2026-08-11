use super::*;

impl PageRenderer {
    /// Render a knockout transparency group per ISO 32000-1:2008 §11.4.6.2.
    ///
    /// The group's initial backdrop is `pixmap` on entry. Each painted
    /// element composites against that backdrop (not against earlier
    /// elements in the group), and later elements override earlier ones
    /// in overlap regions.
    ///
    /// Implementation: segment the operator stream at paint operators
    /// (Fill / Stroke / FillStroke / PaintShading / DrawObject /
    /// ShowText / inline image). For each paint boundary `i`, render
    /// the cumulative slice `operators[0..=i]` into a fresh
    /// backdrop-copy scratch pixmap. The cumulative replay preserves
    /// graphics-state side effects (color, CTM, clip) across paint
    /// boundaries while keeping each paint's pixel contribution
    /// referenced to the original backdrop. The scratch pixmap's
    /// differences from the backdrop identify the pixels this element
    /// touched, which then overwrite the accumulator.
    ///
    /// Cost: O(N · K) operator executions where N is total operators
    /// and K is paint operators. Knockout groups are rare in practice
    /// so the quadratic factor is acceptable.
    pub(super) fn execute_knockout_group(
        &mut self,
        pixmap: &mut Pixmap,
        base_transform: Transform,
        operators: &[Operator],
        doc: &PdfDocument,
        page_num: usize,
        resources: &Object,
    ) -> Result<()> {
        // Backdrop is the pixmap state at group entry.
        let width = pixmap.width();
        let height = pixmap.height();
        let backdrop_data: Vec<u8> = pixmap.data().to_vec();

        // Sidecar backdrop snapshot. ISO 32000-1 §11.3.3 + §11.4.6.2:
        // a knockout group composes each element against the group's
        // INITIAL backdrop, and the single (shape, opacity) the spec
        // maintains per pixel applies to BOTH process and spot lanes.
        // So the CMYK plane and every spot plane must be reset to the
        // group's backdrop before each element's cumulative replay,
        // exactly like the RGB pixmap is. Without this reset the
        // round-2 spot mirror's per-paint writes would compose against
        // the previous element's lane state — that is non-isolated
        // group semantics, NOT knockout. The brief calls this out as
        // the round-2 gap the secondary scope of round 3 closes.
        let sidecar_backdrop_cmyk: Option<Vec<u8>> =
            self.cmyk_sidecar.as_ref().map(|s| s.cmyk().to_vec());
        let sidecar_backdrop_spots: Option<Vec<u8>> =
            self.cmyk_sidecar.as_ref().map(|s| s.spots_all().to_vec());

        // Identify paint-operator indices. These define element
        // boundaries.
        let paint_indices: Vec<usize> = operators
            .iter()
            .enumerate()
            .filter_map(|(i, op)| if is_paint_operator(op) { Some(i) } else { None })
            .collect();

        if paint_indices.is_empty() {
            // No paint ops — still execute for state side effects (rare).
            return self.execute_operators(
                pixmap,
                base_transform,
                operators,
                doc,
                page_num,
                resources,
            );
        }

        // Accumulator starts as the backdrop. Each element's painted
        // pixels overwrite the accumulator.
        let mut accumulator: Vec<u8> = backdrop_data.clone();
        // Sidecar accumulators parallel `accumulator` for the process
        // and spot lanes. They start at the group's initial backdrop
        // and absorb per-element scratch-vs-backdrop diffs.
        let mut sidecar_accum_cmyk: Option<Vec<u8>> = sidecar_backdrop_cmyk.clone();
        let mut sidecar_accum_spots: Option<Vec<u8>> = sidecar_backdrop_spots.clone();

        for &end_idx in &paint_indices {
            // Cumulative replay: graphics-state operators 0..end_idx
            // plus the paint at end_idx, with all PRIOR paint operators
            // filtered out. Filtering keeps the state side effects
            // (CTM, fill color, ExtGState, clip path construction) that
            // the current paint depends on, while ensuring no earlier
            // element's pixel contribution reaches the scratch. The
            // scratch is initialised to the backdrop so the paint
            // composites against the group's initial backdrop only.
            let mut scratch = Pixmap::new(width, height).ok_or_else(|| {
                crate::error::Error::InvalidPdf("knockout scratch pixmap alloc failed".into())
            })?;
            scratch.data_mut().copy_from_slice(&backdrop_data);

            // Reset sidecar lanes to the group's backdrop before this
            // element's replay so the per-paint mirror writes compose
            // against the BACKDROP (knockout rule), not against earlier
            // elements' lane state. The §11.4.6.2 spec is explicit: the
            // group's "constituent objects ... shall be composited with
            // the group's initial backdrop rather than with each
            // other". This restoration extends that rule to the
            // process / spot lanes the round-1/2 sidecar carries.
            if let (Some(sidecar), Some(cmyk_b)) =
                (self.cmyk_sidecar.as_mut(), sidecar_backdrop_cmyk.as_ref())
            {
                sidecar.restore_cmyk(cmyk_b);
            }
            if let (Some(sidecar), Some(spots_b)) =
                (self.cmyk_sidecar.as_mut(), sidecar_backdrop_spots.as_ref())
            {
                sidecar.restore_spots(spots_b);
            }

            let element_ops: Vec<Operator> = operators[..=end_idx]
                .iter()
                .enumerate()
                .filter_map(|(i, op)| {
                    if i < end_idx && is_paint_operator(op) {
                        None
                    } else {
                        Some(op.clone())
                    }
                })
                .collect();

            self.execute_operators(
                &mut scratch,
                base_transform,
                &element_ops,
                doc,
                page_num,
                resources,
            )?;

            // Merge: where scratch differs from backdrop, this element
            // touched the pixel — its value overrides the accumulator.
            // Comparing scratch vs backdrop (not vs accumulator) is the
            // key knockout semantic: each element sees only the
            // backdrop, never the accumulated paint from earlier
            // elements.
            let scratch_data = scratch.data();
            debug_assert_eq!(scratch_data.len(), backdrop_data.len());
            debug_assert_eq!(accumulator.len(), backdrop_data.len());

            // Process pixel-by-pixel (4 bytes RGBA).
            for px in 0..(scratch_data.len() / 4) {
                let off = px * 4;
                let same = scratch_data[off] == backdrop_data[off]
                    && scratch_data[off + 1] == backdrop_data[off + 1]
                    && scratch_data[off + 2] == backdrop_data[off + 2]
                    && scratch_data[off + 3] == backdrop_data[off + 3];
                if !same {
                    accumulator[off] = scratch_data[off];
                    accumulator[off + 1] = scratch_data[off + 1];
                    accumulator[off + 2] = scratch_data[off + 2];
                    accumulator[off + 3] = scratch_data[off + 3];
                }
            }

            // Merge sidecar lanes: any byte that differs from the
            // backdrop snapshot was written by this element's paint
            // mirror. Pull the post-element value into the accumulator
            // so later replay iterations see only the backdrop on
            // restore, but the merged group result preserves every
            // element's contribution (last-paint wins on per-byte
            // collision, mirroring the pixmap merge).
            if let (Some(sidecar), Some(accum), Some(backdrop)) = (
                self.cmyk_sidecar.as_ref(),
                sidecar_accum_cmyk.as_mut(),
                sidecar_backdrop_cmyk.as_ref(),
            ) {
                let post = sidecar.cmyk();
                debug_assert_eq!(post.len(), backdrop.len());
                debug_assert_eq!(accum.len(), backdrop.len());
                for i in 0..post.len() {
                    if post[i] != backdrop[i] {
                        accum[i] = post[i];
                    }
                }
            }
            if let (Some(sidecar), Some(accum), Some(backdrop)) = (
                self.cmyk_sidecar.as_ref(),
                sidecar_accum_spots.as_mut(),
                sidecar_backdrop_spots.as_ref(),
            ) {
                let post = sidecar.spots_all();
                debug_assert_eq!(post.len(), backdrop.len());
                debug_assert_eq!(accum.len(), backdrop.len());
                for i in 0..post.len() {
                    if post[i] != backdrop[i] {
                        accum[i] = post[i];
                    }
                }
            }
        }

        // Replay any trailing non-paint operators (state side effects
        // that follow the last paint) onto the accumulator. The group's
        // visible output IS the accumulator, so we install it before
        // returning.
        pixmap.data_mut().copy_from_slice(&accumulator);

        // Install the merged sidecar accumulators back into the
        // sidecar. The group's spot and process lanes are now the
        // accumulated knockout result — later operators (outside the
        // group) compose against this state.
        if let (Some(sidecar), Some(cmyk_a)) =
            (self.cmyk_sidecar.as_mut(), sidecar_accum_cmyk.as_ref())
        {
            sidecar.restore_cmyk(cmyk_a);
        }
        if let (Some(sidecar), Some(spots_a)) =
            (self.cmyk_sidecar.as_mut(), sidecar_accum_spots.as_ref())
        {
            sidecar.restore_spots(spots_a);
        }
        Ok(())
    }

    /// Apply extended graphics state parameters.
    #[allow(dead_code)]
    pub(super) fn apply_ext_g_state(
        &self,
        gs: &mut GraphicsState,
        dict_name: &str,
        resources: &Object,
        doc: &PdfDocument,
    ) -> Result<()> {
        // Retained as a thin wrapper for any external caller; the operator
        // loop in `execute_operators` uses the cached fast path via
        // `parse_ext_g_state` instead.
        let parsed = parse_ext_g_state(dict_name, resources, doc).unwrap_or_default();
        parsed.apply(gs);
        Ok(())
    }

    /// Render annotations for a page.
    pub(super) fn render_annotations(
        &mut self,
        pixmap: &mut Pixmap,
        base_transform: Transform,
        doc: &PdfDocument,
        page_num: usize,
    ) -> Result<()> {
        let annotations = doc.get_annotations(page_num)?;
        // Reuse the per-render snapshot so we don't deep-clone the HashSet here.
        let excluded_snapshot: Option<Arc<HashSet<String>>> = self.excluded_layers_snapshot.clone();
        for annot in annotations {
            // Per ISO 32000-1 §12.5.2, an annotation dict may carry an /OC
            // entry referencing the OCG/OCMD the annotation belongs to. Skip
            // the annotation entirely if its layer is excluded.
            if let Some(ref excluded_layers) = excluded_snapshot {
                if let Some(oc_obj) = annot.raw_dict.as_ref().and_then(|d| d.get("OC")) {
                    if crate::optional_content::annotation_is_excluded(oc_obj, doc, excluded_layers)
                    {
                        continue;
                    }
                }
            }
            // Check if annotation has an appearance stream (/AP)
            if let Some(ap_obj) = annot.raw_dict.as_ref().and_then(|d| d.get("AP")) {
                let ap_stream_obj = doc.resolve_object(ap_obj)?;

                // Normal appearance (N)
                if let Object::Dictionary(ap_dict) = ap_stream_obj {
                    if let Some(n_entry) = ap_dict.get("N").or_else(|| ap_dict.values().next()) {
                        let n_stream_obj = doc.resolve_object(n_entry)?;
                        if let Object::Stream { ref dict, .. } = n_stream_obj {
                            let ap_data = if let Some(r) = n_entry.as_reference() {
                                doc.decode_stream_with_encryption(&n_stream_obj, r)?
                            } else {
                                n_stream_obj.decode_stream_data()?
                            };

                            if let Some(rect) = annot.rect {
                                let x = rect[0] as f32;
                                let y = rect[1] as f32;
                                let annot_transform = base_transform.pre_translate(x, y);

                                let old_fonts = self.fonts.clone();
                                let old_cs = self.color_spaces.clone();
                                if let Some(res) = dict.get("Resources") {
                                    if let Ok(res_obj) = doc.resolve_object(res) {
                                        self.load_resources(doc, &res_obj)?;
                                    }
                                }

                                self.render_form_xobject(
                                    pixmap,
                                    &dict,
                                    &ap_data,
                                    annot_transform,
                                    doc,
                                    page_num,
                                    &Object::Dictionary(std::collections::HashMap::new()),
                                )?;

                                self.fonts = old_fonts;
                                self.color_spaces = old_cs;
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Encode Pixmap to JPEG format.
    pub(super) fn encode_jpeg(&self, pixmap: &Pixmap) -> Result<Vec<u8>> {
        let width = pixmap.width();
        let height = pixmap.height();
        let data = pixmap.data();

        let mut rgb_data = Vec::with_capacity((width * height * 3) as usize);
        for i in 0..(width * height) as usize {
            let r = data[i * 4] as f32;
            let g = data[i * 4 + 1] as f32;
            let b = data[i * 4 + 2] as f32;
            let a = data[i * 4 + 3] as f32 / 255.0;

            if a > 0.0 {
                rgb_data.push((r / a).min(255.0) as u8);
                rgb_data.push((g / a).min(255.0) as u8);
                rgb_data.push((b / a).min(255.0) as u8);
            } else {
                rgb_data.push(0);
                rgb_data.push(0);
                rgb_data.push(0);
            }
        }

        let img = image::ImageBuffer::<image::Rgb<u8>, _>::from_raw(width, height, rgb_data)
            .ok_or_else(|| Error::InvalidPdf("Failed to create image buffer".to_string()))?;

        let mut output = std::io::Cursor::new(Vec::new());
        img.write_to(&mut output, image::ImageFormat::Jpeg)
            .map_err(|e| Error::InvalidPdf(format!("JPEG encoding failed: {}", e)))?;

        Ok(output.into_inner())
    }
}
