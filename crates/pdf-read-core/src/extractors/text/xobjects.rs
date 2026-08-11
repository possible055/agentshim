use super::*;

impl<'doc> TextExtractor<'doc> {
    /// Resolve XObject name to ObjectRef using cached mapping.
    pub(super) fn resolve_xobject_ref(&mut self, name: &str) -> Result<Option<ObjectRef>> {
        // Check cache first (O(1) lookup)
        if let Some(cached) = self.cached_xobject_refs.get(name) {
            return Ok(*cached);
        }

        // Cache miss — resolve the full chain once and populate cache
        let resources = match &self.resources {
            Some(res) => res.clone(),
            None => return Ok(None),
        };

        let doc = match self.document {
            Some(d) => d,
            None => return Ok(None),
        };

        // Resolve resources → XObject dict
        let resources_obj = if let Some(res_ref) = resources.as_reference() {
            doc.load_object(res_ref)?
        } else {
            resources
        };

        let resources_dict = match resources_obj.as_dict() {
            Some(d) => d,
            None => return Ok(None),
        };

        let xobject_entry = match resources_dict.get("XObject") {
            Some(xobj) => xobj.clone(),
            None => return Ok(None),
        };

        let xobject_obj = if let Some(xobj_ref) = xobject_entry.as_reference() {
            doc.load_object(xobj_ref)?
        } else {
            xobject_entry
        };

        let xobject_dict = match xobject_obj.as_dict() {
            Some(d) => d,
            None => return Ok(None),
        };

        // Populate the entire cache for this resources context
        for (key, val) in xobject_dict.iter() {
            let obj_ref = val.as_reference();
            self.cached_xobject_refs.insert(key.clone(), obj_ref);
        }

        // Return the requested name
        Ok(self.cached_xobject_refs.get(name).copied().flatten())
    }

    pub(super) fn process_xobject(&mut self, name: &str) -> Result<()> {
        if self.xobject_depth >= Self::MAX_XOBJECT_DEPTH {
            return Ok(());
        }
        if self.xobject_decode_count >= Self::MAX_XOBJECT_DECODES {
            return Ok(());
        }

        // Resolve name → ObjectRef using cached mapping (avoids expensive
        // repeated resolution of resources/XObject dict chain)
        let xobject_ref = match self.resolve_xobject_ref(name)? {
            Some(r) => r,
            None => return Ok(()),
        };

        // Build a CTM-aware deduplication key.
        //
        // Using just `xobject_ref` as the key incorrectly blocked re-processing
        // the same Form XObject when it was invoked a second time on the same page
        // with a different CTM (e.g., same header/footer XObject stamped at two
        // different Y positions, or the nougat_005 pattern where each page's
        // content stream sets a different `cm` translation before calling `Do`).
        //
        // The CTM is encoded as 6 millipoint-rounded i64 values so it can be
        // stored in a HashSet without floating-point equality hazards.
        // Infinite-recursion cycles are still prevented because a truly recursive
        // call re-enters with the *same* XObject ref AND the same CTM at that
        // nesting depth; the depth limiter (MAX_XOBJECT_DEPTH) provides a
        // second backstop.
        let current_ctm = self.state_stack.current().ctm;
        // Round to nearest millipoint instead of truncating with `as i64`,
        // so floating-point noise in the same logical CTM produces a
        // stable hash key (truncation alone could send 0.99999...
        // 1.00001... to different buckets).
        let ctm_key = [
            (current_ctm.a * 1000.0).round() as i64,
            (current_ctm.b * 1000.0).round() as i64,
            (current_ctm.c * 1000.0).round() as i64,
            (current_ctm.d * 1000.0).round() as i64,
            (current_ctm.e * 1000.0).round() as i64,
            (current_ctm.f * 1000.0).round() as i64,
        ];
        let xobj_key = (xobject_ref, ctm_key);

        // Skip already-processed (XObject, CTM) pairs — each unique combination
        // is processed at most once per page for text extraction.
        if self.processed_xobjects.contains(&xobj_key) {
            return Ok(());
        }

        self.processed_xobjects.insert(xobj_key);

        // Get document reference for loading objects.
        let doc = match self.document {
            Some(d) => d,
            None => return Ok(()),
        };

        if doc
            .xobject_text_free_cache
            .lock()
            .unwrap()
            .contains(&xobject_ref)
        {
            return Ok(());
        }

        // Quick Subtype check: skip Image XObjects without loading the full object.
        // Image XObjects can be megabytes of compressed pixel data — loading them
        // just to discover Subtype=Image is a major bottleneck (10-15ms per image).
        if !doc.is_form_xobject(xobject_ref) {
            return Ok(());
        }

        // Span result cache: reuse extracted spans from self-contained Form XObjects.
        //
        // The cache key is (ObjectRef, ctm_key) where ctm_key encodes the caller's
        // CTM as 6 millipoint-rounded i64 values. This allows the same Form XObject
        // to have independent cached results for each unique CTM it is painted with,
        // fixing the issue where cross-page reuse of a single Form XObject with
        // different per-page CTM translations returned stale page-0 coordinates on
        // all subsequent pages (nougat_005.pdf, Issue B1).
        //
        // `ctm_key` was already computed above for the `processed_xobjects` guard.
        let spans_cache_key = (xobject_ref, ctm_key);
        let has_filters = !self.excluded_layers.is_empty() || !self.excluded_inks.is_empty();
        if self.extract_spans && !has_filters {
            let cached_spans = {
                doc.xobject_spans_cache
                    .lock()
                    .unwrap()
                    .get(&spans_cache_key)
                    .cloned()
            };
            if let Some(cached_spans) = cached_spans {
                if let Some(spans) = cached_spans {
                    self.spans.extend(spans.iter().cloned());
                }
                return Ok(());
            }
        }

        // Load the XObject (now known to be Form or unknown — worth the full load)
        let xobject = doc.load_object(xobject_ref)?;

        // Check if it's a Form XObject (has Subtype /Form)
        let xobject_dict = match xobject.as_dict() {
            Some(d) => d,
            None => {
                log::debug!("XObject '{}' is not a dictionary", name);
                return Ok(());
            }
        };

        let subtype = xobject_dict.get("Subtype").and_then(|s| s.as_name());

        match subtype {
            Some("Form") => {
                // Form XObject - extract text from it
                log::debug!("Processing Form XObject: {}", name);

                // Pre-decode resource check: if the XObject's own /Resources has
                // neither /Font nor /XObject entries, it cannot render text directly
                // and cannot invoke nested XObjects. Skip it without decoding the
                // stream, which avoids expensive FlateDecode decompression.
                if let Some(xobj_resources) = xobject_dict.get("Resources") {
                    let xobj_res = if let Some(res_ref) = xobj_resources.as_reference() {
                        doc.load_object(res_ref).ok()
                    } else {
                        Some(xobj_resources.clone())
                    };

                    if let Some(ref res_obj) = xobj_res {
                        if let Some(res_dict) = res_obj.as_dict() {
                            let has_font = res_dict.contains_key("Font");
                            let has_xobject = res_dict.contains_key("XObject");
                            if !has_font && !has_xobject {
                                log::debug!(
                                    "Skipping Form XObject '{}': no Font/XObject in Resources",
                                    name
                                );
                                doc.xobject_text_free_cache
                                    .lock()
                                    .unwrap()
                                    .insert(xobject_ref);
                                return Ok(());
                            }
                        }
                    }
                } else {
                    // No Resources at all — XObject inherits page-level fonts but
                    // still must be decoded to check for text operators. However,
                    // Form XObjects that are pure graphics often omit Resources
                    // entirely when they have no font/xobject needs. Check if the
                    // page has any active fonts; if not, skip.
                }

                // Decode the stream — check cache first to avoid repeated FlateDecode.
                self.xobject_decode_count += 1;
                let cached_stream = {
                    doc.xobject_stream_cache
                        .lock()
                        .unwrap()
                        .get(&xobject_ref)
                        .cloned()
                };
                let stream_data = if let Some(cached) = cached_stream {
                    cached.as_ref().clone()
                } else {
                    match doc.decode_stream_with_encryption(&xobject, xobject_ref) {
                        Ok(data) => {
                            crate::document::admit_xobject_stream(doc, xobject_ref, &data);
                            data
                        }
                        Err(e) => {
                            log::warn!(
                                "Failed to decode Form XObject '{}' stream: {}, skipping",
                                name,
                                e
                            );
                            return Ok(());
                        }
                    }
                };

                if !crate::document::PdfDocument::may_contain_text(&stream_data) {
                    log::debug!(
                        "Skipping text-free Form XObject '{}' ({} bytes)",
                        name,
                        stream_data.len()
                    );
                    doc.xobject_text_free_cache
                        .lock()
                        .unwrap()
                        .insert(xobject_ref);
                    return Ok(());
                }

                // Parse /Matrix from Form XObject dict (default: identity per ISO 32000-1 §8.10.1)
                let form_matrix = if let Some(Object::Array(arr)) = xobject_dict.get("Matrix") {
                    let get_f32 = |i: usize| -> f32 {
                        match arr.get(i) {
                            Some(Object::Real(v)) => *v as f32,
                            Some(Object::Integer(v)) => *v as f32,
                            _ => {
                                if i == 0 || i == 3 {
                                    1.0
                                } else {
                                    0.0
                                }
                            }
                        }
                    };
                    Matrix {
                        a: get_f32(0),
                        b: get_f32(1),
                        c: get_f32(2),
                        d: get_f32(3),
                        e: get_f32(4),
                        f: get_f32(5),
                    }
                } else {
                    Matrix::identity()
                };

                // Parse /BBox (form coordinate space) for the §8.10.1 form clip.
                // A form XObject's painting is clipped to its /BBox; text the form
                // draws outside the BBox is invisible in a conformant renderer and
                // must not be extracted. Stored as [x0,y0,x1,y1]; None disables the
                // clip (defensive — /BBox is required, but malformed dicts exist).
                let form_bbox: Option<[f32; 4]> = match xobject_dict.get("BBox") {
                    Some(Object::Array(arr)) if arr.len() >= 4 => {
                        let f = |i: usize| -> Option<f32> {
                            match arr.get(i) {
                                Some(Object::Real(v)) => Some(*v as f32),
                                Some(Object::Integer(v)) => Some(*v as f32),
                                _ => None,
                            }
                        };
                        match (f(0), f(1), f(2), f(3)) {
                            (Some(a), Some(b), Some(c), Some(d))
                                if a.is_finite()
                                    && b.is_finite()
                                    && c.is_finite()
                                    && d.is_finite() =>
                            {
                                // Normalize so [x0,y0] is the min corner.
                                Some([a.min(c), b.min(d), a.max(c), b.max(d)])
                            }
                            _ => None,
                        }
                    }
                    _ => None,
                };

                // Only save/restore fonts+resources when XObject has its own Resources.
                // Avoids expensive HashMap clone for XObjects that inherit page fonts.
                let has_own_resources = xobject_dict.contains_key("Resources");

                let saved_fonts;
                let saved_resources;
                let saved_xobj_cache;

                if has_own_resources {
                    saved_fonts = Some(self.fonts.clone());
                    saved_resources = self.resources.clone();
                    saved_xobj_cache = Some(std::mem::take(&mut self.cached_xobject_refs));

                    // Safety: has_own_resources was set by contains_key("Resources")
                    // so get("Resources") will always return Some here
                    let xobj_resources = xobject_dict
                        .get("Resources")
                        .expect("contains_key confirmed Resources exists");
                    let xobj_res = if let Some(res_ref) = xobj_resources.as_reference() {
                        match doc.load_object(res_ref) {
                            Ok(obj) => obj,
                            Err(_) => xobj_resources.clone(),
                        }
                    } else {
                        xobj_resources.clone()
                    };

                    if let Err(e) = doc.load_fonts(&xobj_res, self) {
                        log::debug!(
                            "Failed to load fonts for Form XObject '{}': {}, using page fonts",
                            name,
                            e
                        );
                    }

                    self.resources = Some(xobj_res);
                } else {
                    saved_fonts = None;
                    saved_resources = None;
                    saved_xobj_cache = None;
                }

                // Track span count for result caching
                let spans_before = self.spans.len();

                // Save graphics state (implicit q per ISO 32000-1 §8.10.1)
                self.state_stack.save();

                // Concatenate Form XObject /Matrix with CTM
                let state = self.state_stack.current_mut();
                state.ctm = form_matrix.multiply(&state.ctm);

                // Effective form→page transform used for every span drawn inside
                // the form; the /BBox clip below maps the BBox through this same
                // CTM so the comparison happens in one coordinate space.
                let form_ctm = self.state_stack.current().ctm;

                // Push the Form XObject scope (ISO 32000-1:2008
                // §14.7.4.3). Every MCID emitted inside this form's
                // content stream lives in the form's MCID namespace,
                // *not* the page's. Two distinct forms on the same
                // page that both emit MCID 0 stay distinct because
                // they push different `Form(form_ref)` scopes.
                self.mcid_scope_stack
                    .push(crate::structure::McidScope::Form(xobject_ref));

                self.xobject_depth += 1;
                let parse_result = if self.excluded_inks.is_empty() {
                    parse_and_execute_text_only(&stream_data, |op| self.execute_operator(op))
                } else {
                    let ops = parse_content_stream(&stream_data);
                    match ops {
                        Ok(ops) => {
                            for op in ops {
                                self.execute_operator(op)?;
                            }
                            Ok(())
                        }
                        Err(e) => Err(e),
                    }
                };
                self.xobject_depth -= 1;
                // Pop the Form XObject scope pushed before the
                // content-stream walk. Cleared regardless of parse
                // success so the parent stream's scope is correctly
                // restored even on errors.
                self.mcid_scope_stack.pop();
                if let Err(e) = parse_result {
                    log::debug!(
                        "Error parsing Form XObject '{}' content stream: {}, partial text may be extracted",
                        name,
                        e
                    );
                }

                // Apply the Form XObject /BBox clip (ISO 32000-1:2008 §8.10.1): a
                // form's marks are clipped to its BBox, so text the form paints
                // OUTSIDE the BBox is invisible in a conformant renderer (pdfium,
                // Acrobat, MuPDF) and must not be extracted. Some producers (e.g.
                // pdfTeX \includegraphics of a figure PDF that retained a full
                // draft-galley page) paint a redundant copy of the article body
                // outside the figure's BBox; without this clip it surfaces as
                // duplicate text overlapping the real page body. Done BEFORE the
                // span cache so cached results are already clipped. Byte-identical
                // on every form whose painted text lies inside its BBox (the
                // conformant majority) — only out-of-BBox marks are dropped.
                if let Some([bx0, by0, bx1, by1]) = form_bbox {
                    if self.spans.len() > spans_before && bx1 > bx0 && by1 > by0 {
                        // Map the BBox corners through the form CTM into page space
                        // and take the axis-aligned bound (a superset for rotated
                        // forms — conservative, never over-clips).
                        let c = [
                            form_ctm.transform_point(bx0, by0),
                            form_ctm.transform_point(bx1, by0),
                            form_ctm.transform_point(bx1, by1),
                            form_ctm.transform_point(bx0, by1),
                        ];
                        let min_x = c.iter().map(|p| p.x).fold(f32::INFINITY, f32::min);
                        let max_x = c.iter().map(|p| p.x).fold(f32::NEG_INFINITY, f32::max);
                        let min_y = c.iter().map(|p| p.y).fold(f32::INFINITY, f32::min);
                        let max_y = c.iter().map(|p| p.y).fold(f32::NEG_INFINITY, f32::max);
                        if min_x.is_finite()
                            && max_x.is_finite()
                            && min_y.is_finite()
                            && max_y.is_finite()
                        {
                            // Tolerance so glyphs sitting exactly on the clip edge
                            // are kept (conformant clipping is exact; this only
                            // guards float rounding, far below any real margin).
                            const TOL: f32 = 1.0;
                            let inside = |s: &TextSpan| {
                                let cx = s.bbox.x + s.bbox.width * 0.5;
                                let cy = s.bbox.y + s.bbox.height * 0.5;
                                cx >= min_x - TOL
                                    && cx <= max_x + TOL
                                    && cy >= min_y - TOL
                                    && cy <= max_y + TOL
                            };
                            // Fast path: when every span this form painted is
                            // already inside its /BBox (the conformant majority —
                            // and where this clip is a no-op anyway), skip the
                            // split_off/extend allocation churn entirely. Only the
                            // rare out-of-BBox case (the draft-galley underlay)
                            // pays for the rebuild. Cheap O(form-spans) scan, no
                            // allocation; keeps large form-heavy docs fast.
                            if self.spans[spans_before..].iter().any(|s| !inside(s)) {
                                // Out-of-BBox spans exist. Distinguish a real
                                // figure form (whose stray out-of-BBox text is a
                                // draft-galley underlay safe to drop) from a
                                // full-page content-frame wrapper whose declared
                                // BBox happens to exclude real body text. A
                                // conformant *renderer* clips both, but every text
                                // *extractor* (poppler/pdftotext, the common
                                // reference) keeps a wrapper's body — and that body
                                // may be its only copy. The discriminator is
                                // coverage: a figure occupies a sub-region of the
                                // page; a wrapper covers most of it. Only clip when
                                // the form is figure-sized, so the galley-dedup win
                                // stays while page-wrapper bodies are preserved.
                                let clip_area = (max_x - min_x) * (max_y - min_y);
                                let page_idx = match self.mcid_scope_stack.first() {
                                    Some(crate::structure::McidScope::Page(p)) => *p as usize,
                                    _ => 0,
                                };
                                let page_area = self
                                    .document
                                    .and_then(|d| d.get_page_media_box(page_idx).ok())
                                    .map(|(llx, lly, urx, ury)| ((urx - llx) * (ury - lly)).abs())
                                    .filter(|a| *a > 0.0);
                                // ≥60% of page area ⇒ content-frame wrapper, not a
                                // figure (figures measured ≤27%; wrappers ≥82%).
                                let is_page_wrapper =
                                    page_area.is_some_and(|pa| clip_area >= 0.6 * pa);
                                if !is_page_wrapper {
                                    let added = self.spans.split_off(spans_before);
                                    let kept: Vec<TextSpan> =
                                        added.into_iter().filter(|s| inside(s)).collect();
                                    self.spans.extend(kept);
                                }
                            }
                        }
                    }
                }

                // Cache span results for self-contained Form XObjects.
                //
                // The cache key `spans_cache_key` already encodes (ObjectRef, ctm_key),
                // so each unique (XObject, CTM) pair gets its own entry. There is no
                // longer any need to restrict caching to identity-CTM invocations —
                // different CTMs produce different cache entries and therefore cannot
                // pollute each other (this was the root cause of issue B1).
                //
                // We still require `has_own_resources` so that font lookups are
                // self-contained; XObjects that inherit page-level fonts would
                // produce spans whose glyph mappings depend on caller context.
                if has_own_resources && self.extract_spans && !has_filters {
                    let new_spans = if self.spans.len() > spans_before {
                        Some(self.spans[spans_before..].to_vec())
                    } else {
                        None
                    };
                    doc.xobject_spans_cache
                        .lock()
                        .unwrap()
                        .insert(spans_cache_key, new_spans);
                }

                // Restore graphics state (implicit Q per ISO 32000-1 §8.10.1)
                self.state_stack.restore();
                // Sync cached font with restored state
                self.cached_current_font = self
                    .state_stack
                    .current()
                    .font_name
                    .as_ref()
                    .and_then(|name| self.fonts.get(name))
                    .cloned();

                // Restore fonts, resources, and XObject cache only if saved
                if let Some(fonts) = saved_fonts {
                    self.fonts = fonts;
                }
                if let Some(res) = saved_resources {
                    self.resources = Some(res);
                }
                if let Some(cache) = saved_xobj_cache {
                    self.cached_xobject_refs = cache;
                }
                // Re-evaluate ink exclusion against the restored color space
                // and resources. The XObject may have set an excluded ink that
                // must not persist into the caller's scope.
                if !self.excluded_inks.is_empty() {
                    let cs = self.state_stack.current().fill_color_space.clone();
                    self.inside_excluded_ink = self.is_excluded_ink_color_space(&cs);
                }

                // Keep xobject_ref in processed_xobjects permanently.
                // For text extraction, re-processing the same Form XObject produces
                // identical text. Keeping it prevents O(n!) fan-out in pages with
                // deep XObject trees (e.g., 4000+ nested chart elements).

                Ok(())
            }
            Some("Image") => {
                // Image XObject - no text to extract
                log::debug!("Skipping Image XObject: {}", name);
                Ok(())
            }
            _ => {
                log::debug!("Unknown XObject subtype for '{}': {:?}", name, subtype);
                Ok(())
            }
        }
    }

    /// Get the current artifact type from the marked content stack.
    pub(super) fn current_artifact_type(&self) -> Option<ArtifactType> {
        self.marked_content_stack
            .iter()
            .rev()
            .find_map(|ctx| ctx.artifact_type.clone())
    }
}
