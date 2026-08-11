use super::*;

impl PageRenderer {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn execute_xobject_operator(
        &mut self,
        op: &Operator,
        pixmap: &mut Pixmap,
        base_transform: Transform,
        gs_stack: &mut GraphicsStateStack,
        clip_stack: &[Option<tiny_skia::Mask>],
        excluded_layer_depth: u32,
        doc: &PdfDocument,
        page_num: usize,
        resources: &Object,
    ) -> Result<bool> {
        match op {
            Operator::Do { name } => {
                if excluded_layer_depth == 0 {
                    let gs_clone = gs_stack.current().clone();
                    let transform = combine_transforms(base_transform, &gs_clone.ctm);
                    let clip = clip_stack.last().and_then(|c| c.as_ref());
                    log::debug!("Do: rendering XObject '{}'", name);
                    // §11.4.7 + §11.7.4 + §11.4 cycle: the entire
                    // XObject paint (Form or Image) sits inside the
                    // snapshot bracket so a /SMask attached via
                    // ExtGState modulates the cumulative
                    // contribution. Image XObjects always behave as
                    // fill-side paints; Form XObjects honour their
                    // own internal ExtGState changes (the snapshot
                    // captures the page-level state, the Form runs
                    // recursively, and the apply blends the Form's
                    // contribution against the captured backdrop).
                    //
                    // Per-subtype dispatch for the post-Do colour-
                    // lane modulators: Image / ImageMask XObjects do
                    // NOT execute their own paint operators — their
                    // pixel data is painted using the outer
                    // graphics state, so the post-Do CMYK compose,
                    // overprint and spot-lane mirrors are how those
                    // lanes learn about the contribution. Form
                    // XObjects DO execute their own paint operators
                    // (Fill / Stroke / FillStroke / Do / ShowText /
                    // shading), each of which runs its own per-
                    // paint sidecar mirror with the FORM's gs at
                    // the time of the paint. Re-applying the outer
                    // gs's CMYK / overprint / spot mirror after a
                    // Form Do would composite the form's region
                    // again with whatever colour the OUTER gs had,
                    // double-counting (and, when the outer colour
                    // differs from the form's, overwriting the
                    // form's mirror writes — the QA-6 / QA-6-DIAG-2
                    // failure mode where outer /K's iteration 2
                    // /Inner Do lost the inner Form's spot
                    // contribution). SMask attenuation always
                    // applies — an outer /SMask gs in effect at the
                    // Do attaches to the Do's entire region
                    // regardless of how the inner produced its
                    // pixels.
                    let xobj_subtype = self.xobject_subtype(name, resources, doc);
                    let is_form = matches!(xobj_subtype.as_deref(), Some("Form"));
                    let smask_snap = self.smask_snapshot(pixmap, &gs_clone);
                    let smask_spot_snap = self.smask_spot_snapshot(&gs_clone);
                    let overprint_snap = if is_form {
                        None
                    } else {
                        self.overprint_snapshot(pixmap, &gs_clone, true)
                    };
                    let cmyk_compose_snap = if is_form {
                        None
                    } else {
                        self.cmyk_compose_snapshot(pixmap, &gs_clone, doc, true)
                    };
                    let spot_snap = if is_form {
                        None
                    } else {
                        self.spot_paint_snapshot(pixmap, &gs_clone, true)
                    };
                    // §8.9.5 + §8.9.6.2 + §11.7.3: rasterise the
                    // Image / ImageMask footprint + stencil-bit
                    // coverage so the spot mirror has a geometry-
                    // true per-pixel mask. Skipped for Form
                    // XObjects (their per-paint mirror runs
                    // inside the recursive content stream — the
                    // post-Do mirror for Forms is already
                    // suppressed by round 3's P0 fix).
                    let image_coverage = spot_snap.as_ref().and_then(|_| {
                        self.rasterise_image_xobject_coverage(
                            name, transform, &gs_clone, resources, doc, clip,
                        )
                    });
                    self.render_xobject(
                        pixmap, name, transform, &gs_clone, resources, doc, page_num, clip,
                    )?;
                    if let Some(snap) = cmyk_compose_snap {
                        self.apply_cmyk_compose_after_paint(pixmap, &snap, &gs_clone, doc, true);
                    }
                    if let Some(snap) = overprint_snap {
                        self.apply_overprint_after_paint(pixmap, &snap, &gs_clone, doc, true);
                    }
                    if let Some(snap) = spot_snap {
                        self.mirror_spot_paint_into_sidecar_with_coverage(
                            pixmap,
                            &snap,
                            image_coverage.as_deref(),
                            &gs_clone,
                            true,
                        );
                    }
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

            // Inline image (`BI ... ID <data> EI`) — §8.9.7. Unlike a
            // `Do`-invoked image XObject, the pixel data sits directly in
            // the content stream (no indirect object, so no encryption
            // and no `/SMask` or `/Mask`, both of which the spec only
            // allows as indirect references). Expand the abbreviated
            // dictionary keys/values into the same shape
            // `extract_image_from_xobject` expects, wrap it in a
            // synthetic `Object::Stream`, and paint it through the same
            // `render_image`/`render_image_mask` used for `Do` images.
            Operator::InlineImage { dict, data } => {
                if excluded_layer_depth == 0 {
                    let gs_clone = gs_stack.current().clone();
                    let transform = combine_transforms(base_transform, &gs_clone.ctm);
                    let clip = clip_stack.last().and_then(|c| c.as_ref());
                    let expanded =
                        crate::extractors::images::expand_inline_image_dict((**dict).clone());
                    let is_image_mask = expanded
                        .get("ImageMask")
                        .map(|o| matches!(o, Object::Boolean(true)))
                        .unwrap_or(false);
                    let synthetic = Object::Stream {
                        dict: expanded,
                        data: bytes::Bytes::from(data.clone()),
                    };
                    if is_image_mask {
                        if let Err(e) = self.render_image_mask(
                            pixmap, &synthetic, None, transform, doc, clip, &gs_clone,
                        ) {
                            log::warn!("Skipping unrenderable inline ImageMask: {}", e);
                        }
                    } else if let Err(e) = self.render_image(
                        pixmap, &synthetic, None, transform, doc, clip, None, None, &gs_clone,
                    ) {
                        log::warn!("Skipping unrenderable inline image: {}", e);
                    }
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }
}
