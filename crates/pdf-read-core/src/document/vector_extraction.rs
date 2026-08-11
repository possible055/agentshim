use super::parsing::*;
use super::preflight::*;
use super::*;

impl PdfDocument {
    /// Extract path (vector graphics) content from a page.
    ///
    /// This extracts all vector graphics operations from the page's content stream,
    /// including lines, curves, rectangles, and shapes.
    ///
    /// # Arguments
    ///
    /// * `page_index` - Zero-based page index
    ///
    /// # Returns
    ///
    /// A vector of `PathContent` objects representing all paths on the page.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use pdf_oxide::document::PdfDocument;
    /// # fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let mut doc = PdfDocument::open("example.pdf")?;
    ///
    /// // Extract paths from first page
    /// let paths = doc.extract_paths(0)?;
    ///
    /// for path in paths {
    ///     println!("Path with {} operations, bbox: {:?}",
    ///         path.operations.len(), path.bbox);
    ///     if path.has_stroke() {
    ///         println!(" Stroked with width: {}", path.stroke_width);
    ///     }
    ///     if path.has_fill() {
    ///         println!(" Filled");
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn extract_paths(&self, page_index: usize) -> Result<Vec<crate::elements::PathContent>> {
        use crate::content::{parse_content_stream_paths_only, Operator};
        use crate::elements::{LineCap, LineJoin};
        use crate::extractors::paths::{FillRule, PathExtractor, PathGraphicsStateStack};
        use crate::layout::Color;

        // Get page object and content stream
        let page = self.get_page(page_index)?;
        let page_dict = page.as_dict().ok_or_else(|| Error::ParseError {
            offset: 0,
            reason: "Page is not a dictionary".to_string(),
        })?;

        // Get content stream data — skip page on decode failure (Annex I)
        let content_data = match self.get_page_content_data(page_index) {
            Ok(data) => data,
            Err(e) => {
                log::warn!(
                    "Failed to decode content stream for page {}: {}, returning empty paths",
                    page_index,
                    e
                );
                return Ok(Vec::new());
            }
        };

        let operators = match parse_content_stream_paths_only(&content_data) {
            Ok(ops) => ops,
            Err(e) => {
                log::warn!(
                    "Failed to parse content stream for page {}: {}, returning empty paths",
                    page_index,
                    e
                );
                return Ok(Vec::new());
            }
        };

        let mut extractor = PathExtractor::new();
        let mut state_stack = PathGraphicsStateStack::new();

        // Resolve and set page resources for XObject processing
        if let Some(resources) = page_dict.get("Resources") {
            let resolved_resources = if let Some(ref_obj) = resources.as_reference() {
                self.load_object(ref_obj)?
            } else {
                resources.clone()
            };
            extractor.set_resources(resolved_resources);
        }

        // Process each operator
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
                    let new_matrix = crate::content::Matrix { a, b, c, d, e, f };
                    // PDF spec ISO 32000-1:2008 §8.3.4: cm concatenates as M_cm × CTM
                    state.ctm = new_matrix.multiply(&state.ctm);
                    extractor.set_ctm(state.ctm);
                }

                // Color operators (stroke)
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

                // Color operators (fill)
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

                // Line style operators
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
                Operator::MoveTo { x, y } => {
                    extractor.move_to(x, y);
                }
                Operator::LineTo { x, y } => {
                    extractor.line_to(x, y);
                }
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
                Operator::ClosePath => {
                    extractor.close_path();
                }

                // Path painting operators
                Operator::Stroke => {
                    extractor.stroke();
                }
                Operator::Fill => {
                    extractor.fill(FillRule::NonZero);
                }
                Operator::FillEvenOdd => {
                    extractor.fill(FillRule::EvenOdd);
                }
                Operator::CloseFillStroke => {
                    extractor.close_fill_and_stroke(FillRule::NonZero);
                }
                Operator::EndPath => {
                    extractor.end_path();
                }

                // Clipping operators
                Operator::ClipNonZero => {
                    extractor.clip_non_zero();
                }
                Operator::ClipEvenOdd => {
                    extractor.clip_even_odd();
                }

                // XObject processing
                Operator::Do { name } => {
                    if let Err(e) =
                        self.process_form_xobject_paths(&name, &mut extractor, &mut state_stack)
                    {
                        log::warn!(
                            "Failed to process XObject '{}' in path extraction: {}",
                            name,
                            e
                        );
                    }
                }

                // Marked content operators — maintain the active Optional
                // Content Group (PDF "layer") so each finalized path gets
                // tagged with the OCG it was emitted under. Per ISO 32000-1
                // §14.6, every `BDC`/`BMC` must be balanced by an `EMC`,
                // so we always push (with `None` for non-`/OC` tags) and
                // always pop — keeps the stack depth in sync with the
                // marked-content nesting.
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

                // Skip other operators (text, images, etc.)
                _ => {}
            }
        }

        Ok(extractor.finish())
    }

    /// Resolve a `BDC /OC <properties>` property operand to the human-readable
    /// layer name of the Optional Content it refers to (PDF spec
    /// ISO 32000-1:2008 §8.11, §14.6).
    ///
    /// `properties` is the operand parsed by `Operator::BeginMarkedContentDict`
    /// — per spec it is either:
    ///
    /// 1. An inline dictionary: an OCG (or OCMD) — read its name directly.
    /// 2. A name (e.g. `/MC0`) that references `<resources> /Properties
    ///    <name>` → an OCG or OCMD dictionary → read its name.
    ///
    /// `resources` is the resource dictionary currently in scope: the page
    /// `/Resources` at page level, or the active Form XObject's own
    /// `/Resources` when extracting inside an XObject (§14.6.2, §8.10.1).
    ///
    /// Returns `None` for malformed PDFs, missing `/Resources /Properties`
    /// entries, or optional-content objects without a resolvable name.
    /// Callers treat `None` as "path belongs to no named layer" — extraction
    /// continues normally.
    pub(super) fn resolve_oc_layer_name(
        &self,
        resources: Option<&crate::object::Object>,
        properties: &crate::object::Object,
    ) -> Option<String> {
        const OC_NAME_MAX_DEPTH: u8 = 8;

        // Case 1: inline dictionary — the property list itself is the OCG (or
        // OCMD) dictionary.
        if let Some(dict) = properties.as_dict() {
            return self.read_oc_name(dict, OC_NAME_MAX_DEPTH);
        }

        // Case 2: name reference (e.g. `/MC0`) — resolve through the current
        // resource dict's `/Properties` subdictionary.
        let prop_name = properties.as_name()?;
        let resources_obj = self.deref_object(resources?)?;
        let properties_dict = resources_obj.as_dict()?.get("Properties")?;
        let properties_obj = self.deref_object(properties_dict)?;
        let target = properties_obj.as_dict()?.get(prop_name)?;
        let target_obj = self.deref_object(target)?;
        self.read_oc_name(target_obj.as_dict()?, OC_NAME_MAX_DEPTH)
    }

    /// Read the human-readable layer name from an Optional Content dictionary.
    ///
    /// - An **OCG** (§8.11.2.1) carries its label in `/Name` — a PDF *text
    ///   string*, decoded via [`Self::decode_pdf_text_string`] so
    ///   PDFDocEncoding (Annex D) and UTF-16 (BE/LE, with BOM) layer names
    ///   round-trip identically to the rest of the library.
    /// - An **OCMD** (§8.11.3.2, Table 99) has no `/Name` of its own; its
    ///   member OCGs live in `/OCGs`, which is *either* a single OCG *or* an
    ///   array of them (array entries may be `null`). We follow the first
    ///   entry that resolves to a dictionary and read its name.
    ///
    /// `depth` bounds the `/OCGs` chain so a malformed PDF whose membership
    /// dictionary points back to another OCMD cannot recurse forever.
    /// Returns `None` for missing / non-dictionary / nameless inputs — the
    /// path is simply left unlabelled.
    pub(super) fn read_oc_name(
        &self,
        dict: &std::collections::HashMap<String, crate::object::Object>,
        depth: u8,
    ) -> Option<String> {
        use crate::object::Object;

        if depth == 0 {
            return None;
        }

        // OCMD: no /Name of its own — follow /OCGs to the first member OCG.
        if matches!(dict.get("Type").and_then(|t| t.as_name()), Some("OCMD")) {
            let ocgs = self.deref_object(dict.get("OCGs")?)?;
            let first_ocg = match ocgs.as_array() {
                // /OCGs as an array: first entry that derefs to a dictionary.
                Some(entries) => entries
                    .iter()
                    .find_map(|e| self.deref_object(e).filter(|o| o.as_dict().is_some())),
                // /OCGs as a single OCG (already a dictionary).
                None => Some(ocgs.clone()),
            };
            return self.read_oc_name(first_ocg?.as_dict()?, depth - 1);
        }

        // OCG (or inline property dict): /Name is a PDF text string.
        match dict.get("Name")? {
            Object::String(bytes) => Some(Self::decode_pdf_text_string(bytes)),
            // Tolerate a /Name written as a PDF name object (non-conformant,
            // but seen in real exports).
            Object::Name(s) => Some(s.clone()),
            _ => None,
        }
    }

    /// Dereference one level of indirection, loading the target object;
    /// pass direct objects through unchanged. `None` if a reference fails to
    /// load — callers treat that as "unresolvable, leave unlabelled".
    pub(super) fn deref_object(
        &self,
        obj: &crate::object::Object,
    ) -> Option<crate::object::Object> {
        match obj.as_reference() {
            Some(r) => self.load_object(r).ok(),
            None => Some(obj.clone()),
        }
    }

    /// Extract rectangles from a page (v0.3.14).
    ///
    /// Identifies paths that form axis-aligned rectangles.
    pub fn extract_rects(&self, page_index: usize) -> Result<Vec<crate::elements::PathContent>> {
        let paths = self.extract_paths(page_index)?;
        Ok(paths.into_iter().filter(|p| p.is_rectangle()).collect())
    }

    /// Extract straight lines from a page (v0.3.14).
    ///
    /// Identifies paths that form a single straight line segment.
    pub fn extract_lines(&self, page_index: usize) -> Result<Vec<crate::elements::PathContent>> {
        let paths = self.extract_paths(page_index)?;
        Ok(paths.into_iter().filter(|p| p.is_straight_line()).collect())
    }

    /// Extract tables from a page (v0.3.14).
    ///
    /// Uses a hybrid spatial algorithm that combines text alignment and vector lines
    /// for robust table detection without explicit structure markup.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let tables = doc.extract_tables(0)?;
    /// for table in tables {
    ///     println!("Table with {} rows and {} columns", table.rows.len(), table.col_count);
    /// }
    /// ```
    pub fn extract_tables(
        &self,
        page_index: usize,
    ) -> Result<Vec<crate::structure::table_extractor::Table>> {
        self.extract_tables_with_config(
            page_index,
            crate::structure::spatial_table_detector::TableDetectionConfig::default(),
        )
    }

    /// Extract tables from a page using a custom configuration (v0.3.14).
    pub fn extract_tables_with_config(
        &self,
        page_index: usize,
        config: crate::structure::spatial_table_detector::TableDetectionConfig,
    ) -> Result<Vec<crate::structure::table_extractor::Table>> {
        use crate::structure::spatial_table_detector::detect_tables_with_lines;

        // Use words instead of spans for better granularity.
        // This ensures that strings with spaces are split into separate columns
        // for the spatial detector.
        let words = self.extract_words(page_index)?;
        // Use all table primitives (lines, rectangles, borders) not just straight lines
        let lines: Vec<_> = self
            .extract_paths(page_index)?
            .into_iter()
            .filter(|p| p.is_table_primitive())
            .collect();

        // Convert Words to TextSpans for the spatial detector
        let spans: Vec<_> = words
            .into_iter()
            .map(|w| crate::layout::TextSpan {
                provenance: None,
                artifact_type: None,
                text: w.text,
                bbox: w.bbox,
                font_name: w.dominant_font,
                font_size: w.avg_font_size,
                font_weight: if w.is_bold {
                    crate::layout::FontWeight::Bold
                } else {
                    crate::layout::FontWeight::Normal
                },
                is_italic: w.is_italic,
                is_monospace: false,
                color: crate::layout::Color::black(),
                mcid: w.mcid,
                mcid_scope: None,
                sequence: 0,
                split_boundary_before: false,
                offset_semantic: false,
                char_spacing: 0.0,
                word_spacing: 0.0,
                horizontal_scaling: 1.0,
                primary_detected: false,
                char_widths: vec![],
                char_x_offsets: Vec::new(),
                heading_level: None,
                rotation_degrees: 0.0,
                wmode: 0,
                text_rise: 0.0,
                rtl_draw_logical: false,
            })
            .collect();

        // Same prose-rejection filter `extract_page_tables` applies to the
        // extract_text/to_markdown/to_html path — this public API called
        // `detect_tables_with_lines` directly with no post-filter at all, so
        // it was already able to fabricate/garble tables on any prose-shaped
        // spatial candidate, independent of anything else in this function.
        Ok(detect_tables_with_lines(&spans, &lines, &config)
            .into_iter()
            .filter(|t| t.is_real_grid() && !looks_like_prose_table(t))
            .collect())
    }
}
