use super::*;

impl<'doc> TextExtractor<'doc> {
    /// Execute a single operator.
    ///
    /// Updates the graphics state and extracts text as appropriate.
    pub(super) fn execute_operator(&mut self, op: Operator) -> Result<()> {
        self.budget_checkpoint()?;
        match op {
            // Text state operators
            Operator::Tf { font, size } => {
                // Skip flush + lookup when font name AND size haven't changed.
                // Many PDFs redundantly set the same font (e.g., Tf after q/Q).
                let same_font = {
                    let state = self.state_stack.current();
                    state.font_size == size && state.font_name.as_deref() == Some(font.as_str())
                };
                if !same_font {
                    // Flush Tj buffer before changing font — the buffer decodes bytes
                    // using the font set at creation time, so a font change requires a
                    // new buffer to avoid decoding with the wrong ToUnicode CMap.
                    self.flush_tj_span_buffer()?;

                    // Cache font reference for advance_position_for_string
                    self.cached_current_font = self.fonts.get(&font).cloned();
                    // Cache wmode on the graphics state so the advance hot
                    // path branches on a single primitive read instead of
                    // dereferencing the FontInfo every glyph.
                    let new_wmode = self
                        .cached_current_font
                        .as_deref()
                        .map(|f| f.wmode)
                        .unwrap_or(0);

                    let state = self.state_stack.current_mut();
                    state.font_name = Some(font);
                    state.font_size = size;
                    state.text_wmode = new_wmode;
                }
            }

            // Text positioning operators
            Operator::Tm { a, b, c, d, e, f } => {
                // Optimization: batch character-by-character Tm+Tj patterns.
                // Many PDFs position each character with individual Tm+Tj operators.
                // If the new Tm is on the same line with the same transform,
                // keep accumulating into the existing buffer instead of flushing
                // (avoids creating thousands of 1-char TextSpans per page).
                // When merge_tm_tj_runs is false, every Tm always starts a fresh span.
                //
                // #518: glyph-jitter tolerance. Microsoft Word emits each
                // glyph in its own `BT Tm Tj ET` block with ±2.5–5pt
                // sinusoidal baseline jitter for broken-image placeholder
                // text. ISO 32000-1 §9.4 leaves logical reading order to
                // the extractor, so a baseline delta far smaller than the
                // line's own height is the SAME visual line — only a
                // delta on the order of the font size is a real line
                // break (body leading ≳ 1.0× font size). The previous
                // `f.round() as i32 ==` check tolerated only ±0.5pt
                // split jittered glyphs into separate Y-banded spans that
                // the reading-order sort then scrambled. Tolerance is
                // scale-relative (0.5× the text-space glyph height, ≥0.5pt
                // floor) so it is correct at any font size and still
                // splits genuine line breaks.
                let cur_font_size = self.state_stack.current().font_size;
                let is_continuation = self.merging_config.merge_tm_tj_runs
                    && match self.tj_span_buffer {
                        Some(ref mut buffer)
                            if !buffer.is_empty()
                                && (f - buffer.start_matrix.f).abs()
                                    <= ((cur_font_size * buffer.start_matrix.d).abs() * 0.5)
                                        .max(0.5)
                                && a == buffer.start_matrix.a
                                && b == buffer.start_matrix.b
                                && c == buffer.start_matrix.c
                                && d == buffer.start_matrix.d
                                && e >= buffer.start_matrix.e =>
                        {
                            // Same line, same transform, LTR progression →
                            // update width to reflect actual visual extent
                            buffer.accumulated_width = e - buffer.start_matrix.e;
                            true
                        }
                        _ => false,
                    };

                if !is_continuation {
                    self.flush_tj_span_buffer()?;
                }

                let state = self.state_stack.current_mut();
                state.text_matrix = Matrix { a, b, c, d, e, f };
                state.text_line_matrix = state.text_matrix;
            }
            Operator::Td { tx, ty } => {
                // Flush Tj buffer before changing text position
                self.flush_tj_span_buffer()?;
                let state = self.state_stack.current_mut();
                // Per ISO 32000-1:2008 §9.4.2, Table 108:
                // Tlm_new = T(tx,ty) × Tlm_old
                // The translation is in text-line space, so it must be
                // pre-multiplied to be scaled by the existing Tlm transform.
                let tm = Matrix::translation(tx, ty);
                state.text_line_matrix = tm.multiply(&state.text_line_matrix);
                state.text_matrix = state.text_line_matrix;
            }
            Operator::TD { tx, ty } => {
                // Flush Tj buffer before changing text position
                self.flush_tj_span_buffer()?;

                // TD is like Td but also sets leading
                let state = self.state_stack.current_mut();
                state.leading = -ty;
                // Per ISO 32000-1:2008 §9.4.2: Tlm_new = T(tx,ty) × Tlm_old
                let tm = Matrix::translation(tx, ty);
                state.text_line_matrix = tm.multiply(&state.text_line_matrix);
                state.text_matrix = state.text_line_matrix;
            }
            Operator::TStar => {
                // Flush Tj buffer before moving to next line
                self.flush_tj_span_buffer()?;

                // Move to start of next line (using leading)
                let leading = self.state_stack.current().leading;
                let state = self.state_stack.current_mut();
                // Per ISO 32000-1:2008 §9.4.2: Tlm_new = T(0,-TL) × Tlm_old
                let tm = Matrix::translation(0.0, -leading);
                state.text_line_matrix = tm.multiply(&state.text_line_matrix);
                state.text_matrix = state.text_line_matrix;
            }

            // Text showing operators
            Operator::Tj { text } => {
                // Note: We do NOT skip /Artifact content here.
                // Many PDFs incorrectly mark page content as artifacts.
                // For tagged PDFs, the structure tree already excludes artifacts
                // via MCID mapping, so no filtering is needed at extractor level.

                // ActualText override
                // Per PDF Spec ISO 32000-1:2008, Section 14.9.4:
                // ActualText provides replacement text for the marked-content
                // SEQUENCE — emitted ONCE, no matter how many Tj operators
                // sit inside. The peek/mark pair below handles both first-Tj
                // (emit replacement) and subsequent-Tj (suppress entirely,
                // advance only) cases.
                let (current_at, already_emitted) = self.peek_current_actual_text();
                if let Some(actual_text) = current_at {
                    if already_emitted {
                        // Subsequent show-text inside the same MC scope:
                        // glyphs are already covered by the one replacement
                        // that fired on the first Tj. Advance positioning so
                        // any later, OUTER-scope show-text lands correctly,
                        // but emit nothing.
                        let w = self.advance_position_for_string(&text)?;
                        if let Some(ref mut buffer) = self.tj_span_buffer {
                            buffer.accumulated_width += w;
                        }
                    } else {
                        log::debug!(
                            "Tj operator: emitting MC-scope ActualText '{}'",
                            actual_text
                        );
                        self.mark_actual_text_emitted();
                        if self.extract_spans {
                            // Use ActualText in span mode — push pre-decoded
                            // Unicode directly into the buffer, bypassing
                            // font character mapping.
                            if self.tj_span_buffer.is_none() {
                                self.tj_span_buffer = Some(TjBuffer::new(
                                    self.state_stack.current(),
                                    self.current_mcid,
                                    self.cached_current_font.clone(),
                                ));
                            }
                            if let Some(ref mut buffer) = self.tj_span_buffer {
                                buffer.unicode.push_str(&actual_text);
                            }
                        } else {
                            // Character mode: show_text maps through font, but ActualText
                            // is already decoded. Fall back to show_text for positioning.
                            self.show_text(actual_text.as_bytes())?;
                        }
                        // Advance position for the original text (to maintain layout)
                        let w = self.advance_position_for_string(&text)?;
                        if let Some(ref mut buffer) = self.tj_span_buffer {
                            buffer.accumulated_width += w;
                        }
                    }
                } else {
                    // No ActualText - use standard text extraction
                    if self.extract_spans {
                        // NEW: Buffer consecutive Tj operators into single spans
                        // Per PDF Spec ISO 32000-1:2008, Section 9.4.4 NOTE 6:
                        // "text strings are as long as possible"

                        // Create buffer if doesn't exist
                        if self.tj_span_buffer.is_none() {
                            self.tj_span_buffer = Some(TjBuffer::new(
                                self.state_stack.current(),
                                self.current_mcid,
                                self.cached_current_font.clone(),
                            ));
                        }

                        // Merged single-pass: Unicode decode + width + position advance
                        self.append_and_advance(&text)?;
                    } else {
                        self.show_text(&text)?;
                    }
                }
            }
            Operator::TJ { array } => {
                // Note: We do NOT skip /Artifact content here.
                // Many PDFs incorrectly mark page content as artifacts.
                // For tagged PDFs, the structure tree already excludes artifacts
                // via MCID mapping, so no filtering is needed at extractor level.

                // ActualText override
                // Per PDF Spec ISO 32000-1:2008, Section 14.9.4:
                // The MC-scope `/ActualText` replaces the ENTIRE sequence
                // exactly once — see the Tj path above for the per-scope
                // peek/mark protocol that handles both first and
                // subsequent show-text operators inside the same scope.
                let (current_at, already_emitted) = self.peek_current_actual_text();
                if let Some(actual_text) = current_at {
                    if !already_emitted {
                        log::debug!(
                            "TJ operator: emitting MC-scope ActualText '{}' (replacing {} elements)",
                            actual_text,
                            array.len()
                        );
                        self.mark_actual_text_emitted();
                        if self.extract_spans {
                            let mut buffer = TjBuffer::new(
                                self.state_stack.current(),
                                self.current_mcid,
                                self.cached_current_font.clone(),
                            );
                            buffer.unicode.push_str(&actual_text);
                            self.flush_tj_buffer(buffer)?;
                        } else {
                            self.show_text(actual_text.as_bytes())?;
                        }
                    }
                    // First or subsequent: advance position for the
                    // entire TJ array so layout stays consistent.
                    for element in array {
                        match element {
                            TextElement::String(s) => {
                                let w = self.advance_position_for_string(&s)?;
                                if let Some(ref mut buffer) = self.tj_span_buffer {
                                    buffer.accumulated_width += w;
                                }
                            }
                            TextElement::Offset(offset) => {
                                self.advance_position_for_offset(offset)?;
                            }
                        }
                    }
                } else {
                    // No ActualText - use standard TJ array processing
                    if self.extract_spans {
                        // NEW: Use buffered TJ array processing for span extraction
                        // Per PDF Spec ISO 32000-1:2008, Section 9.4.4 NOTE 6:
                        // "text strings are as long as possible"
                        // This creates one span per logical text unit instead of fragmenting
                        self.process_tj_array(&array)?;
                    } else {
                        // Keep old behavior for character extraction mode
                        for element in array {
                            match element {
                                TextElement::String(s) => {
                                    self.show_text(&s)?;
                                }
                                TextElement::Offset(offset) => {
                                    // Adjust text position by offset (in thousandths of em)
                                    let state = self.state_stack.current();
                                    let tx = -offset / 1000.0
                                        * state.font_size
                                        * state.horizontal_scaling
                                        / 100.0;

                                    // HEURISTIC: Insert space character for significant negative offsets
                                    //
                                    // PDF Spec Reference: ISO 32000-1:2008, Section 9.4.4
                                    // The spec defines text positioning but does NOT specify when a positioning
                                    // offset represents a word boundary vs. tight kerning.
                                    //
                                    // In PDFs, spaces are often represented as negative positioning offsets in TJ arrays,
                                    // not as explicit space characters. For example:
                                    // [(Text1) -200 (Text2)] TJ <- the -200 creates visual spacing
                                    //
                                    // Geometry-based adaptive threshold (based on font metrics)
                                    // Formula: adaptive_threshold = -(average_glyph_width * word_margin_ratio)
                                    // This adapts to different font sizes and families.
                                    // Fallback: static threshold if font unavailable or adaptive disabled.
                                    let threshold = self.calculate_adaptive_tj_threshold();
                                    if offset < threshold {
                                        let text_matrix = state.text_matrix;
                                        let ctm = state.ctm;
                                        let font_name = state.font_name.clone();
                                        let font_size = state.font_size;
                                        let fill_color_rgb = state.fill_color_rgb;

                                        // Calculate effective font size (accounting for CTM and text matrix scaling)
                                        let combined = ctm.multiply(&text_matrix);
                                        let effective_font_size = font_size
                                            * (combined.d * combined.d + combined.b * combined.b)
                                                .sqrt();

                                        // Get font for determining weight
                                        let font = font_name
                                            .as_ref()
                                            .and_then(|name| self.fonts.get(name));
                                        let font_weight = if let Some(font) = font {
                                            if font.is_bold() {
                                                FontWeight::Bold
                                            } else {
                                                FontWeight::Normal
                                            }
                                        } else {
                                            FontWeight::Normal
                                        };

                                        // Create space character at current position
                                        // Apply CTM to get position in user space
                                        let text_pos = text_matrix.transform_point(0.0, 0.0);
                                        let pos = ctm.transform_point(text_pos.x, text_pos.y);
                                        let (r, g, b) = fill_color_rgb;
                                        let is_italic_space = font_name
                                            .as_ref()
                                            .and_then(|name| self.fonts.get(name))
                                            .map(|font| font.is_italic())
                                            .unwrap_or(false);
                                        let font_name_str = font_name.unwrap_or_default();
                                        // Compose CTM and text_matrix for full transformation
                                        let final_matrix = ctm.multiply(&text_matrix);
                                        // Calculate rotation from matrix: atan2(b, a)
                                        let rotation_degrees =
                                            final_matrix.b.atan2(final_matrix.a).to_degrees();

                                        let space_char = TextChar {
                                            char: ' ',
                                            bbox: Rect::new(
                                                pos.x,               // X position in user space
                                                pos.y,               // Y position in user space
                                                tx.abs(), // Width = the gap being created
                                                effective_font_size, // Height = effective font size
                                            ),
                                            font_name: font_name_str,
                                            font_size: effective_font_size,
                                            font_weight,
                                            color: Color::new(r, g, b),
                                            mcid: self.current_mcid,
                                            is_italic: is_italic_space,
                                            is_monospace: false,
                                            // Transformation properties (v0.3.1)
                                            origin_x: pos.x,
                                            origin_y: pos.y,
                                            rotation_degrees,
                                            advance_width: tx.abs(),
                                            rendered_advance: tx.abs(),
                                            ascent: font.map(|f| f.ascent).unwrap_or(0.95)
                                                * effective_font_size,
                                            descent: font.map(|f| f.descent).unwrap_or(-0.35)
                                                * effective_font_size,
                                            matrix: Some([
                                                final_matrix.a,
                                                final_matrix.b,
                                                final_matrix.c,
                                                final_matrix.d,
                                                final_matrix.e,
                                                final_matrix.f,
                                            ]),
                                        };
                                        if !self.is_content_suppressed() {
                                            self.chars.push(space_char);
                                        }
                                    }

                                    // Route through advance_text_matrix so the
                                    // axis swap (H vs V) lives in one place.
                                    // Per ISO 32000-1 §9.4.4 a TJ numeric
                                    // offset shifts along the active writing
                                    // axis: x for WMode 0, y for WMode 1.
                                    self.state_stack.current_mut().advance_text_matrix(tx);
                                }
                            }
                        }
                    }
                }
            }
            Operator::Quote { text } => {
                // ' operator: Move to next line (T*) and show text (Tj)
                // Flush any pending span buffer before line break
                self.flush_tj_span_buffer()?;

                let leading = self.state_stack.current().leading;
                {
                    let state = self.state_stack.current_mut();
                    // Per ISO 32000-1:2008 §9.4.2: Tlm_new = T(0,-TL) × Tlm_old
                    let tm = Matrix::translation(0.0, -leading);
                    state.text_line_matrix = tm.multiply(&state.text_line_matrix);
                    state.text_matrix = state.text_line_matrix;
                }

                if self.extract_spans {
                    if self.tj_span_buffer.is_none() {
                        self.tj_span_buffer = Some(TjBuffer::new(
                            self.state_stack.current(),
                            self.current_mcid,
                            self.cached_current_font.clone(),
                        ));
                    }
                    self.append_and_advance(&text)?;
                } else {
                    self.show_text(&text)?;
                }
            }
            Operator::DoubleQuote {
                word_space,
                char_space,
                text,
            } => {
                // " operator: Set spacing, move to next line (T*), and show text (Tj)
                // Flush any pending span buffer before line break
                self.flush_tj_span_buffer()?;

                {
                    let state = self.state_stack.current_mut();
                    state.word_space = word_space;
                    state.char_space = char_space;
                    let leading = state.leading;
                    // Per ISO 32000-1:2008 §9.4.2: Tlm_new = T(0,-TL) × Tlm_old
                    let tm = Matrix::translation(0.0, -leading);
                    state.text_line_matrix = tm.multiply(&state.text_line_matrix);
                    state.text_matrix = state.text_line_matrix;
                }

                if self.extract_spans {
                    if self.tj_span_buffer.is_none() {
                        self.tj_span_buffer = Some(TjBuffer::new(
                            self.state_stack.current(),
                            self.current_mcid,
                            self.cached_current_font.clone(),
                        ));
                    }
                    self.append_and_advance(&text)?;
                } else {
                    self.show_text(&text)?;
                }
            }

            // Text state parameters
            Operator::Tc { char_space } => {
                self.state_stack.current_mut().char_space = char_space;
            }
            Operator::Tw { word_space } => {
                self.state_stack.current_mut().word_space = word_space;
            }
            Operator::Tz { scale } => {
                self.state_stack.current_mut().horizontal_scaling = scale;
            }
            Operator::TL { leading } => {
                self.state_stack.current_mut().leading = leading;
            }
            Operator::Ts { rise } => {
                self.state_stack.current_mut().text_rise = rise;
            }
            Operator::Tr { render } => {
                self.state_stack.current_mut().render_mode = render;
            }

            // Graphics state operators
            Operator::SaveState => {
                // Flush the Tj span buffer before pushing graphics state.
                // q/Q wraps a graphics-state block; restoring after Q can
                // re-set the CTM to an earlier value, leaving the
                // captured user_pos inside the buffer out of sync with
                // the active CTM. Flush so each q/Q block emits its
                // own clean cluster.
                self.flush_tj_span_buffer()?;
                self.state_stack.save();
            }
            Operator::RestoreState => {
                self.flush_tj_span_buffer()?;
                self.state_stack.restore();
                // Sync cached font with restored state
                self.cached_current_font = self
                    .state_stack
                    .current()
                    .font_name
                    .as_ref()
                    .and_then(|name| self.fonts.get(name))
                    .cloned();
                // Re-evaluate ink exclusion for the restored color space
                if !self.excluded_inks.is_empty() {
                    let cs = self.state_stack.current().fill_color_space.clone();
                    self.inside_excluded_ink = self.is_excluded_ink_color_space(&cs);
                }
            }
            Operator::Cm { a, b, c, d, e, f } => {
                // Flush the Tj span buffer before changing the CTM.
                // The buffer captured `user_pos_x`/`user_pos_y` and
                // `user_h_scale` from the CTM in effect when it was
                // created (TjBuffer::new at the first Tj after BT).
                // Non-conforming PDFs can issue cm operators inside
                // a text object — typically when figure / chart text
                // runs alternate `cm` for position with text
                // operators in the same BT/ET block. Without a
                // flush, subsequent Tj chars get a position derived
                // from the new CTM while the buffer still reports
                // the stale `user_pos`, dropping the cluster off
                // the page in the worst case. Flushing here emits
                // the current cluster at its captured position and
                // the next Tj creates a fresh buffer under the new
                // CTM. Spec basis: §9.4 lists cm as general
                // graphics state, not formally allowed inside
                // BT/ET, but conforming readers must process it.
                self.flush_tj_span_buffer()?;
                let state = self.state_stack.current_mut();
                let new_ctm = Matrix { a, b, c, d, e, f };
                // PDF spec ISO 32000-1:2008 §8.3.4: cm concatenates as M_cm × CTM
                state.ctm = new_ctm.multiply(&state.ctm);
            }

            // Color operators
            color_operator @ (Operator::SetFillRgb { .. }
            | Operator::SetStrokeRgb { .. }
            | Operator::SetFillGray { .. }
            | Operator::SetStrokeGray { .. }
            | Operator::SetFillCmyk { .. }
            | Operator::SetStrokeCmyk { .. }
            | Operator::SetFillColorSpace { .. }
            | Operator::SetStrokeColorSpace { .. }
            | Operator::SetFillColor { .. }
            | Operator::SetStrokeColor { .. }
            | Operator::SetFillColorN { .. }
            | Operator::SetStrokeColorN { .. }) => {
                self.execute_color_operator(color_operator)?;
            }
            Operator::SetLineCap { cap_style } => {
                self.state_stack.current_mut().line_cap = cap_style;
            }
            Operator::SetLineJoin { join_style } => {
                self.state_stack.current_mut().line_join = join_style;
            }
            Operator::SetMiterLimit { limit } => {
                self.state_stack.current_mut().miter_limit = limit;
            }
            Operator::SetRenderingIntent { intent } => {
                self.state_stack.current_mut().rendering_intent = intent.clone();
            }
            Operator::SetFlatness { tolerance } => {
                self.state_stack.current_mut().flatness = tolerance;
            }
            Operator::SetExtGState { dict_name } => {
                // ExtGState operator - set graphics state from resource dictionary
                // PDF Spec: ISO 32000-1:2008, Section 8.4.5
                //
                // This operator references an ExtGState dictionary in the page resources
                // that contains transparency, blend modes, and other graphics state parameters.
                //
                // For now, we log the usage. Full implementation would require:
                // 1. Access to page resources (/ExtGState dictionary)
                // 2. Loading the named dictionary
                // 3. Extracting /CA (fill alpha), /ca (stroke alpha), /BM (blend mode), etc.
                // 4. Updating graphics state accordingly
                //
                // Future enhancement: Pass resources to text extractor for full support
                log::debug!(
                    "ExtGState '{}' referenced (transparency/blend modes not yet fully supported)",
                    dict_name
                );

                // Apply default transparency values for now
                // In a full implementation, we would look up dict_name in resources
                // and apply the actual values from the ExtGState dictionary
            }
            Operator::PaintShading { name } => {
                // Shading operator - paint gradient/shading pattern
                // PDF Spec: ISO 32000-1:2008, Section 8.7.4.3
                //
                // Shading patterns define smooth color gradients and can be:
                // Type 1: Function-based shading
                // Type 2: Axial shading (linear gradient)
                // Type 3: Radial shading (circular gradient)
                // Type 4-7: Mesh-based shadings (Gouraud, Coons patch, tensor-product)
                //
                // For text extraction, shading patterns don't affect text content.
                // Full implementation would require rendering the gradient for visual output.
                log::debug!(
                    "Shading pattern '{}' referenced (gradients not rendered in text extraction)",
                    name
                );
            }
            Operator::InlineImage { dict, data } => {
                // Inline image operator - embedded image in content stream
                // PDF Spec: ISO 32000-1:2008, Section 8.9.7 - Inline Images
                //
                // Inline images are small images embedded directly in the content stream
                // using the BI...ID...EI sequence, rather than referenced as XObjects.
                //
                // For text extraction, inline images don't contribute to text content.
                // They would be rendered for visual output or extracted separately
                // for image extraction functionality.
                //
                // Common dictionary keys (abbreviated):
                // - W: Width, H: Height
                // - CS: ColorSpace (DeviceRGB, DeviceGray, etc.)
                // - BPC: BitsPerComponent
                // - F: Filter (FlateDecode, DCTDecode, etc.)
                let width = dict
                    .get("W")
                    .and_then(|obj| match obj {
                        Object::Integer(i) => Some(*i),
                        _ => None,
                    })
                    .unwrap_or(0);
                let height = dict
                    .get("H")
                    .and_then(|obj| match obj {
                        Object::Integer(i) => Some(*i),
                        _ => None,
                    })
                    .unwrap_or(0);
                log::debug!(
                    "Inline image encountered: {}x{} pixels, {} bytes of data (not rendered in text extraction)",
                    width,
                    height,
                    data.len()
                );
            }

            // Text object operators (BT/ET)
            // PDF Spec ISO 32000-1:2008, Section 9.4.1:
            // "At the beginning of a text object, Tm and Tlm shall be
            // initialized to the identity matrix."
            Operator::BeginText => {
                let state = self.state_stack.current_mut();
                state.text_matrix = Matrix::identity();
                state.text_line_matrix = Matrix::identity();
            }
            Operator::EndText => {
                // Flush any pending text buffer at end of text object
                self.flush_tj_span_buffer()?;
            }

            // Marked content operators - for tagged PDF structure
            // PDF Spec: ISO 32000-1:2008, Section 14.6 - Marked Content
            // These operators define logical structure and accessibility metadata.
            // Per PDF Spec Section 14.6, we track artifact status to filter out
            // non-text content (headers, footers, watermarks, resource paths).
            marked_content_operator @ (Operator::BeginMarkedContent { .. }
            | Operator::BeginMarkedContentDict { .. }
            | Operator::EndMarkedContent) => {
                self.execute_marked_content_operator(marked_content_operator)?;
            }
            Operator::Do { name } => {
                // Flush the Tj span buffer before invoking a Form XObject.
                // `process_xobject` applies the form's /Matrix to the CTM
                // (§8.10.1) and may execute cm/Tm operators inside the
                // form's content stream. The buffer's captured user_pos
                // would no longer correspond to the CTM in effect when
                // the form's text is emitted, so subsequent Tj chars
                // would be stitched into the wrong cluster.
                self.flush_tj_span_buffer()?;

                // Process Form XObjects to extract text from reusable content.
                // Form XObjects can contain text that is not duplicated in the main stream.
                // We track processed XObjects to avoid infinite loops and duplicates.
                if let Err(e) = self.process_xobject(&name) {
                    // Log error but continue processing - don't fail the entire extraction
                    log::warn!("Failed to process XObject '{}': {}", name, e);
                }
            }

            // Other operators we don't need for text extraction
            _ => {
                // Ignore path, image, and other operators
            }
        }

        Ok(())
    }

    /// Maximum XObject recursion depth. Text content in PDFs is rarely nested
    /// more than 2-3 levels. Deep nesting typically indicates complex vector
    /// graphics (charts, plots) with no text content.
    pub(super) const MAX_XOBJECT_DEPTH: u32 = 10;

    pub(super) const MAX_XOBJECT_DECODES: u32 = 500;
}
