use super::parsing::*;
use super::preflight::*;
use super::*;

impl PdfDocument {
    /// Process paths from a Form XObject.
    ///
    /// This method recursively extracts paths from Form XObjects encountered via the `Do` operator.
    /// It handles:
    /// - XObject resolution from resources
    /// - Type checking (Form vs Image)
    /// - Stream decoding and operator parsing
    /// - Coordinate transformations via /Matrix
    /// - Graphics state isolation
    ///
    /// # Arguments
    ///
    /// * `name` - The XObject name from the `Do` operator
    /// * `extractor` - The path extractor to accumulate paths
    /// * `state_stack` - The graphics state stack for transformations
    pub(super) fn process_form_xobject_paths(
        &self,
        name: &str,
        extractor: &mut crate::extractors::paths::PathExtractor,
        state_stack: &mut crate::extractors::paths::PathGraphicsStateStack,
    ) -> Result<()> {
        use crate::content::{parse_content_stream_paths_only, Matrix, Operator};
        use crate::elements::{LineCap, LineJoin};
        use crate::extractors::paths::FillRule;
        use crate::layout::Color;

        let xobject_ref =
            match extractor.resolve_xobject_ref(name, |ref_obj| self.load_object(ref_obj)) {
                Some(r) => r,
                None => return Ok(()),
            };

        // Cycle detection
        if !extractor.can_process_xobject(xobject_ref) {
            return Ok(());
        }
        extractor.push_xobject(xobject_ref);

        // Load XObject
        let xobject = match self.load_object(xobject_ref) {
            Ok(obj) => obj,
            Err(e) => {
                extractor.pop_xobject_failed();
                return Err(e);
            }
        };
        let xobject_dict = match xobject.as_dict() {
            Some(dict) => dict,
            None => {
                extractor.pop_xobject_failed();
                return Err(Error::ParseError {
                    offset: 0,
                    reason: "XObject is not a dictionary".to_string(),
                });
            }
        };

        // Check type - only process Form XObjects, skip Images
        match xobject_dict.get("Subtype") {
            Some(subtype_obj) => {
                if let Some(subtype_name) = subtype_obj.as_name() {
                    if subtype_name != "Form" {
                        extractor.pop_xobject();
                        return Ok(()); // Not a Form XObject, skip
                    }
                } else {
                    extractor.pop_xobject();
                    return Ok(());
                }
            }
            None => {
                extractor.pop_xobject();
                return Ok(());
            }
        }

        // Decode stream — reuse document-level cache shared with text extraction.
        let cached_stream = {
            self.xobject_stream_cache
                .lock_or_recover()
                .get(&xobject_ref)
                .cloned()
        };
        let stream_data = if let Some(cached) = cached_stream {
            cached.as_ref().clone()
        } else {
            match self.decode_stream_with_encryption(&xobject, xobject_ref) {
                Ok(data) => {
                    admit_xobject_stream(self, xobject_ref, &data);
                    data
                }
                Err(e) => {
                    extractor.pop_xobject_failed();
                    return Err(e);
                }
            }
        };

        let operators = match parse_content_stream_paths_only(&stream_data) {
            Ok(ops) => ops,
            Err(e) => {
                extractor.pop_xobject_failed();
                return Err(e);
            }
        };

        // Get transformation matrix (default to identity)
        let matrix = if let Some(matrix_obj) = xobject_dict.get("Matrix") {
            if let Some(array) = matrix_obj.as_array() {
                if array.len() >= 6 {
                    let mut matrix = Matrix::identity();
                    let mut values = [0.0f32; 6];
                    let mut valid = true;

                    for (i, val) in array.iter().take(6).enumerate() {
                        let num = if let Some(f) = val.as_real() {
                            f as f32
                        } else if let Some(i_val) = val.as_integer() {
                            i_val as f32
                        } else {
                            valid = false;
                            break;
                        };
                        values[i] = num;
                    }

                    if valid {
                        matrix.a = values[0];
                        matrix.b = values[1];
                        matrix.c = values[2];
                        matrix.d = values[3];
                        matrix.e = values[4];
                        matrix.f = values[5];
                        matrix
                    } else {
                        Matrix::identity()
                    }
                } else {
                    Matrix::identity()
                }
            } else {
                Matrix::identity()
            }
        } else {
            Matrix::identity()
        };

        // Save graphics state
        state_stack.save();

        // Finalize any pending path before processing XObject to isolate state
        if extractor.has_current_path() {
            extractor.end_path();
        }

        // Apply XObject transformation to CTM
        // PDF spec ISO 32000-1:2008 §8.10.1: Form XObject Matrix concatenates as M × CTM
        let state = state_stack.current_mut();
        state.ctm = matrix.multiply(&state.ctm);
        extractor.set_ctm(state.ctm);

        // Switch resource scope to this Form XObject's own /Resources, if any.
        // Form XObjects with their own Resources define a fresh XObject name
        // scope (ISO 32000-1 §8.10.1). Looking up nested `Do` names against the
        // parent scope can pick up unrelated sibling forms with colliding
        // names, which turns sibling Form XObjects into a cross-recursive tree
        // (O(N!) traversals and unbounded path accumulation).
        let saved_scope = if let Some(xobj_resources) = xobject_dict.get("Resources") {
            let resolved = if let Some(res_ref) = xobj_resources.as_reference() {
                self.load_object(res_ref)
                    .unwrap_or_else(|_| xobj_resources.clone())
            } else {
                xobj_resources.clone()
            };
            Some(extractor.swap_resources(Some(resolved)))
        } else {
            None
        };

        // Remember the marked-content nesting depth on entry so we can drop
        // anything this XObject leaves unbalanced (see truncate below).
        let oc_base_depth = extractor.oc_layer_depth();

        // Process operators from the XObject
        for op in operators {
            match op {
                // Graphics state operators
                Operator::SaveState => {
                    state_stack.save();
                }
                Operator::RestoreState => {
                    state_stack.restore();
                    extractor.update_from_path_state(state_stack.current());
                }
                Operator::Cm { a, b, c, d, e, f } => {
                    let state = state_stack.current_mut();
                    let new_matrix = Matrix { a, b, c, d, e, f };
                    // PDF spec ISO 32000-1:2008 §8.3.4: cm concatenates as M_cm × CTM
                    state.ctm = new_matrix.multiply(&state.ctm);
                    extractor.set_ctm(state.ctm);
                }

                // Color and line style operators — must update both state_stack
                // and extractor so q/Q save/restore works correctly.
                Operator::SetStrokeRgb { r, g, b } => {
                    state_stack.current_mut().stroke_color_rgb = (r, g, b);
                    extractor.set_stroke_color(Color::new(r, g, b));
                }
                Operator::SetStrokeGray { gray } => {
                    state_stack.current_mut().stroke_color_rgb = (gray, gray, gray);
                    extractor.set_stroke_color(Color::new(gray, gray, gray));
                }
                Operator::SetStrokeCmyk { c, m, y, k } => {
                    let (r, g, b) = crate::color::cmyk_to_rgb(c, m, y, k);
                    state_stack.current_mut().stroke_color_rgb = (r, g, b);
                    extractor.set_stroke_color(Color::new(r, g, b));
                }
                Operator::SetFillRgb { r, g, b } => {
                    state_stack.current_mut().fill_color_rgb = (r, g, b);
                    extractor.set_fill_color(Color::new(r, g, b));
                }
                Operator::SetFillGray { gray } => {
                    state_stack.current_mut().fill_color_rgb = (gray, gray, gray);
                    extractor.set_fill_color(Color::new(gray, gray, gray));
                }
                Operator::SetFillCmyk { c, m, y, k } => {
                    let (r, g, b) = crate::color::cmyk_to_rgb(c, m, y, k);
                    state_stack.current_mut().fill_color_rgb = (r, g, b);
                    extractor.set_fill_color(Color::new(r, g, b));
                }
                Operator::SetLineWidth { width } => {
                    state_stack.current_mut().line_width = width;
                    extractor.set_line_width(width);
                }
                Operator::SetLineCap { cap_style } => {
                    state_stack.current_mut().line_cap = cap_style;
                    let cap = match cap_style {
                        1 => LineCap::Round,
                        2 => LineCap::Square,
                        _ => LineCap::Butt,
                    };
                    extractor.set_line_cap(cap);
                }
                Operator::SetLineJoin { join_style } => {
                    state_stack.current_mut().line_join = join_style;
                    let join = match join_style {
                        1 => LineJoin::Round,
                        2 => LineJoin::Bevel,
                        _ => LineJoin::Miter,
                    };
                    extractor.set_line_join(join);
                }

                // Path construction operators
                Operator::MoveTo { x, y } => extractor.move_to(x, y),
                Operator::LineTo { x, y } => extractor.line_to(x, y),
                Operator::CurveTo {
                    x1,
                    y1,
                    x2,
                    y2,
                    x3,
                    y3,
                } => {
                    extractor.curve_to(x1, y1, x2, y2, x3, y3);
                }
                Operator::CurveToV { x2, y2, x3, y3 } => {
                    extractor.curve_to_v(x2, y2, x3, y3);
                }
                Operator::CurveToY { x1, y1, x3, y3 } => {
                    extractor.curve_to_y(x1, y1, x3, y3);
                }
                Operator::Rectangle {
                    x,
                    y,
                    width,
                    height,
                } => {
                    extractor.rectangle(x, y, width, height);
                }
                Operator::ClosePath => extractor.close_path(),

                // Path painting operators
                Operator::Stroke => extractor.stroke(),
                Operator::Fill => extractor.fill(FillRule::NonZero),
                Operator::FillEvenOdd => extractor.fill(FillRule::EvenOdd),
                Operator::CloseFillStroke => extractor.close_fill_and_stroke(FillRule::NonZero),
                Operator::EndPath => extractor.end_path(),

                // Clipping operators
                Operator::ClipNonZero => extractor.clip_non_zero(),
                Operator::ClipEvenOdd => extractor.clip_even_odd(),

                // Nested XObjects (recurse)
                Operator::Do { name: nested_name } => {
                    if let Err(e) =
                        self.process_form_xobject_paths(&nested_name, extractor, state_stack)
                    {
                        log::warn!("Failed to process nested XObject '{}': {}", nested_name, e);
                    }
                }

                // Marked content — same Optional Content Group ("layer")
                // tracking as the page-level loop, but `/OC` property
                // references resolve against *this* XObject's resource scope
                // (swapped in above), per §14.6.2 + §8.10.1. CAD exports that
                // reuse Form XObjects for repeated symbols (gridline labels,
                // callouts) carry their `/OC` markers and local `/Properties`
                // here rather than on the page.
                Operator::BeginMarkedContent { .. } => {
                    extractor.push_oc_layer(None);
                }
                Operator::BeginMarkedContentDict { tag, properties } => {
                    let layer = if tag == "OC" {
                        self.resolve_oc_layer_name(extractor.current_resources(), &properties)
                    } else {
                        None
                    };
                    extractor.push_oc_layer(layer);
                }
                Operator::EndMarkedContent => {
                    extractor.pop_oc_layer();
                }

                // Skip other operators
                _ => {}
            }
        }

        // Finalize any pending path to prevent state leakage
        if extractor.has_current_path() {
            extractor.end_path();
        }

        // Drop any marked-content entries this XObject left open so an
        // unbalanced `BDC` cannot leak its layer onto the caller's paths.
        extractor.truncate_oc_layers(oc_base_depth);

        // Restore the caller's resource scope before popping the cycle guard.
        if let Some(saved) = saved_scope {
            extractor.restore_resources(saved);
        }

        // Restore graphics state
        state_stack.restore();
        extractor.update_from_path_state(state_stack.current());

        // Pop from XObject processing stack
        extractor.pop_xobject();

        Ok(())
    }

    /// Extract paths from a specific rectangular region of a page.
    ///
    /// Only paths whose bounding box intersects the specified region are returned.
    ///
    /// # Arguments
    ///
    /// * `page_index` - Zero-based page index
    /// * `region` - The rectangular region to extract from
    ///
    /// # Returns
    ///
    /// A vector of `PathContent` objects within the specified region.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use pdf_oxide::document::PdfDocument;
    /// # use pdf_oxide::geometry::Rect;
    /// # fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let mut doc = PdfDocument::open("example.pdf")?;
    ///
    /// // Extract paths from a specific region (e.g., header area)
    /// let header_region = Rect::new(0.0, 700.0, 612.0, 92.0);
    /// let paths = doc.extract_paths_in_rect(0, header_region)?;
    ///
    /// println!("Found {} paths in header region", paths.len());
    /// # Ok(())
    /// # }
    /// ```
    pub fn extract_paths_in_rect(
        &self,
        page_index: usize,
        region: crate::geometry::Rect,
    ) -> Result<Vec<crate::elements::PathContent>> {
        let paths = self.extract_paths(page_index)?;

        // Filter paths by region intersection against RENDERED extents: a
        // region query answers "what does the reader see here", so a rule
        // whose drawn bar crosses the region must match even when its
        // geometric bbox is a distant speck. Identical to the
        // geometric test for unstroked paths.
        Ok(paths
            .into_iter()
            .filter(|path| path.rendered_bbox().intersects(&region))
            .collect())
    }

    /// Extract text from a specific rectangular region of a page (v0.3.14).
    ///
    /// Only spans whose bounding boxes match `region` under `mode` are kept;
    /// the retained spans are assembled through the full text pipeline
    /// (reading order, tables, line breaks) so the output matches the
    /// quality of [`Self::extract_text`]. Calling this with a region that covers
    /// the whole page is equivalent to [`Self::extract_text`].
    pub fn extract_text_in_rect(
        &self,
        page_index: usize,
        region: crate::geometry::Rect,
        mode: crate::layout::RectFilterMode,
    ) -> Result<String> {
        let options = crate::converters::ConversionOptions {
            extract_tables: true,
            include_region: Some((region, mode)),
            ..Default::default()
        };
        self.extract_text_with_options(page_index, &options)
    }

    /// Extract words from a specific rectangular region of a page (v0.3.14).
    pub fn extract_words_in_rect(
        &self,
        page_index: usize,
        region: crate::geometry::Rect,
        mode: crate::layout::RectFilterMode,
    ) -> Result<Vec<crate::layout::Word>> {
        use crate::layout::SpatialCollectionFiltering;
        let words = self.extract_words(page_index)?;
        Ok(words.filter_by_rect(&region, mode))
    }

    /// Extract text lines from a specific rectangular region of a page (v0.3.14).
    pub fn extract_text_lines_in_rect(
        &self,
        page_index: usize,
        region: crate::geometry::Rect,
        mode: crate::layout::RectFilterMode,
    ) -> Result<Vec<crate::layout::TextLine>> {
        use crate::layout::SpatialCollectionFiltering;
        let lines = self.extract_text_lines(page_index)?;
        Ok(lines.filter_by_rect(&region, mode))
    }

    /// Extract text spans from a specific rectangular region of a page (v0.3.14).
    pub fn extract_spans_in_rect(
        &self,
        page_index: usize,
        region: crate::geometry::Rect,
        mode: crate::layout::RectFilterMode,
    ) -> Result<Vec<crate::layout::TextSpan>> {
        use crate::layout::SpatialCollectionFiltering;
        let spans = self.extract_spans(page_index)?;
        Ok(spans.filter_by_rect(&region, mode))
    }

    /// Extract text from a page excluding specific rectangular regions.
    ///
    /// The excluded spans are removed before the full text-assembly pipeline
    /// runs, so the output has the same structure — line breaks, tables,
    /// reading order — as [`Self::extract_text`]. Calling this with an empty
    /// `exclude` slice is equivalent to [`Self::extract_text`].
    ///
    /// `mode` controls the overlap rule:
    /// - [`crate::layout::RectFilterMode::Intersects`] (default): drop any span with *any* overlap
    /// - [`crate::layout::RectFilterMode::FullyContained`]: drop only spans lying entirely inside
    /// - `RectFilterMode::MinOverlap(t)`: drop spans where at least fraction `t`
    ///   of the *span's* area overlaps an excluded region
    ///
    /// For Tagged PDFs the extractor already honours `/Artifact` marked-content
    /// (PDF spec §14.8.2.2). This method provides the same capability for
    /// untagged PDFs where spatial coordinates are the only available signal.
    /// Exclusion is unconditional: spans inside a region are dropped regardless
    /// of their structure-tree role.
    pub fn extract_text_excluding_rects(
        &self,
        page_index: usize,
        exclude: &[crate::geometry::Rect],
        mode: crate::layout::RectFilterMode,
    ) -> Result<String> {
        let options = crate::converters::ConversionOptions {
            extract_tables: true,
            exclude_regions: exclude.to_vec(),
            exclude_regions_mode: mode,
            ..Default::default()
        };
        self.extract_text_with_options(page_index, &options)
    }

    /// Extract words from a page excluding specific rectangular regions.
    ///
    /// See [`Self::extract_text_excluding_rects`] for a description of `exclude` and `mode`.
    /// Returns the low-level [`crate::layout::Word`] stream; use [`Self::extract_text_excluding_rects`]
    /// for fully-assembled text with line breaks and tables.
    pub fn extract_words_excluding_rects(
        &self,
        page_index: usize,
        exclude: &[crate::geometry::Rect],
        mode: crate::layout::RectFilterMode,
    ) -> Result<Vec<crate::layout::Word>> {
        use crate::layout::SpatialCollectionFiltering;
        let words = self.extract_words(page_index)?;
        Ok(words.exclude_rects(exclude, mode))
    }

    /// Extract text spans from a page excluding specific rectangular regions.
    ///
    /// See [`Self::extract_text_excluding_rects`] for a description of `exclude` and `mode`.
    /// Returns raw [`crate::layout::TextSpan`] objects with bounding boxes and font metadata;
    /// use [`Self::extract_text_excluding_rects`] for fully-assembled text output.
    pub fn extract_spans_excluding_rects(
        &self,
        page_index: usize,
        exclude: &[crate::geometry::Rect],
        mode: crate::layout::RectFilterMode,
    ) -> Result<Vec<crate::layout::TextSpan>> {
        use crate::layout::SpatialCollectionFiltering;
        let spans = self.extract_spans(page_index)?;
        Ok(spans.exclude_rects(exclude, mode))
    }

    /// Extract rectangles from a specific rectangular region of a page (v0.3.14).
    pub fn extract_rects_in_rect(
        &self,
        page_index: usize,
        region: crate::geometry::Rect,
    ) -> Result<Vec<crate::elements::PathContent>> {
        let rects = self.extract_rects(page_index)?;
        // Rendered extents, matching `extract_paths_in_rect`.
        Ok(rects
            .into_iter()
            .filter(|p| p.rendered_bbox().intersects(&region))
            .collect())
    }

    /// Extract straight lines from a specific rectangular region of a page (v0.3.14).
    pub fn extract_lines_in_rect(
        &self,
        page_index: usize,
        region: crate::geometry::Rect,
    ) -> Result<Vec<crate::elements::PathContent>> {
        let lines = self.extract_lines(page_index)?;
        // Rendered extents, matching `extract_paths_in_rect`: a
        // stroke-width-encoded rule must match region queries over its drawn
        // bar, not only over its geometric speck.
        Ok(lines
            .into_iter()
            .filter(|p| p.rendered_bbox().intersects(&region))
            .collect())
    }

    /// Extract individual characters from a specific rectangular region of a page (v0.3.14).
    pub fn extract_chars_in_rect(
        &self,
        page_index: usize,
        region: crate::geometry::Rect,
        mode: crate::layout::RectFilterMode,
    ) -> Result<Vec<crate::layout::TextChar>> {
        use crate::layout::SpatialCollectionFiltering;
        let chars = self.extract_chars(page_index)?;
        Ok(chars.filter_by_rect(&region, mode))
    }

    /// Extract images from a specific rectangular region of a page (v0.3.14).
    pub fn extract_images_in_rect(
        &self,
        page_index: usize,
        region: crate::geometry::Rect,
    ) -> Result<Vec<crate::extractors::PdfImage>> {
        let images = self.extract_images(page_index)?;
        Ok(images
            .into_iter()
            .filter(|img| {
                if let Some(bbox) = img.bbox() {
                    bbox.intersects(&region)
                } else {
                    false
                }
            })
            .collect())
    }

    /// Extract tables from a specific rectangular region of a page (v0.3.14).
    pub fn extract_tables_in_rect(
        &self,
        page_index: usize,
        region: crate::geometry::Rect,
    ) -> Result<Vec<crate::structure::table_extractor::Table>> {
        self.extract_tables_in_rect_with_config(
            page_index,
            region,
            crate::structure::spatial_table_detector::TableDetectionConfig::relaxed(),
        )
    }

    /// Extract tables from a specific region using custom configuration (v0.3.14).
    pub fn extract_tables_in_rect_with_config(
        &self,
        page_index: usize,
        region: crate::geometry::Rect,
        config: crate::structure::spatial_table_detector::TableDetectionConfig,
    ) -> Result<Vec<crate::structure::table_extractor::Table>> {
        let tables = self.extract_tables_with_config(page_index, config)?;
        Ok(tables
            .into_iter()
            .filter(|table| {
                if let Some(bbox) = table.bbox {
                    bbox.intersects(&region)
                } else {
                    false
                }
            })
            .collect())
    }

    /// Compute a cheap content-based font identity hash from a loaded font object.
    /// Uses only inline fields (no reference resolution / load_object calls) to keep
    /// the cost at ~200ns. Relies on BaseFont + Subtype + Encoding (when inline) to
    /// uniquely identify fonts within a document. For reference-only fields (ToUnicode,
    /// FontDescriptor, DescendantFonts), hashes their presence to avoid false positives
    /// between fonts with vs without these features.
    /// `font_identity_hash_cheap` of `font_ref`'s object, memoized (an object's
    /// content is fixed within a document).
    pub(super) fn cached_font_identity_hash(&self, font_ref: ObjectRef) -> Option<u64> {
        if let Some(&h) = self.font_id_hash_cache.lock_or_recover().get(&font_ref) {
            return Some(h);
        }
        let font = self.load_object(font_ref).ok()?;
        let h = self.font_identity_hash_with_descendants(&font);
        self.font_id_hash_cache
            .lock_or_recover()
            .insert(font_ref, h);
        Some(h)
    }
}
