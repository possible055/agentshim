use super::*;

impl<'doc> TextExtractor<'doc> {
    /// Flush accumulated TJ buffer into a single TextSpan.
    ///
    /// This creates one span for the entire buffer content, properly calculating
    /// the total width including character spacing (Tc) and word spacing (Tw).
    pub(super) fn flush_tj_buffer(&mut self, mut buffer: TjBuffer) -> Result<()> {
        if buffer.is_empty() {
            return Ok(());
        }

        // Use accumulated width from advance_position_for_string calls
        // Convert from text space to user space using pre-computed horizontal scale
        let total_width = buffer.accumulated_width * buffer.user_h_scale;

        // Use pre-computed values from buffer creation (avoids
        // matrix multiply + sqrt + HashMap lookup + transform_point per flush)
        let effective_font_size = buffer.effective_font_size;
        let font_weight = buffer.font_weight;
        let is_italic_span = buffer.is_italic;

        // Move owned strings out of buffer (avoids clone)
        let font_name_span = buffer
            .font_name
            .take()
            .unwrap_or_else(|| "Unknown".to_string());

        // RTL text correction (#826): use the confidence-gated geometric
        // detector (#537) when `char_widths` gives us per-character user-space
        // x-positions, falling back to the coarse "buffer's net horizontal
        // advance is positive" heuristic only for genuinely ambiguous/short
        // runs. Mirrors `flush_tj_span_buffer`'s handling — this used to be
        // the one flush site still on the pre-#537 `accumulated_width > 0.0`
        // check, which (since `accumulated_width` only ever sums *positive*
        // glyph widths — TJ kerning offsets never subtract from it) is true
        // for nearly every non-empty RTL buffer and so was unconditionally
        // reversing every RTL run regardless of its actual source order.
        let mut text = std::mem::take(&mut buffer.unicode);
        if text.len() > 1 {
            let has_rtl = text
                .chars()
                .any(|c| crate::text::rtl_detector::is_rtl_text(c as u32));
            if has_rtl {
                let chars: Vec<char> = text.chars().collect();
                let verdict = if chars.len() == buffer.char_widths.len()
                    && !buffer.char_widths.is_empty()
                {
                    let mut chars_with_x: Vec<(char, f32)> = Vec::with_capacity(chars.len());
                    let mut cursor_text_space = 0.0_f32;
                    for (i, c) in chars.iter().enumerate() {
                        let user_x = buffer.user_pos_x + cursor_text_space * buffer.user_h_scale;
                        chars_with_x.push((*c, user_x));
                        cursor_text_space += buffer.char_widths[i];
                    }
                    crate::text::bidi::detect_visual_order_run(&chars_with_x)
                } else {
                    crate::text::bidi::RunOrder::Ambiguous
                };
                text = crate::text::bidi::apply_rtl_verdict(
                    &text,
                    verdict,
                    buffer.accumulated_width > 0.0,
                    matches!(buffer.render_mode, 3 | 7),
                );
            }
        }

        let span = TextSpan {
            provenance: None,
            text,
            bbox: Rect {
                x: buffer.user_pos_x,
                y: buffer.user_pos_y,
                width: total_width,
                height: effective_font_size,
            },
            font_name: font_name_span,
            font_size: effective_font_size,
            font_weight,
            color: Color::new(
                buffer.fill_color_rgb.0,
                buffer.fill_color_rgb.1,
                buffer.fill_color_rgb.2,
            ),
            mcid: buffer.mcid,
            mcid_scope: Some(self.current_mcid_scope()),
            sequence: self.span_sequence_counter,
            split_boundary_before: false,
            offset_semantic: false,
            char_spacing: buffer.char_space, // Tc - captured from PDF content stream
            word_spacing: buffer.word_space, // Tw - captured from PDF content stream
            horizontal_scaling: buffer.horizontal_scaling, // Tz - captured from PDF content stream
            is_italic: is_italic_span,
            is_monospace: buffer.is_monospace,
            primary_detected: false,
            artifact_type: self.current_artifact_type(),
            char_widths: {
                let mut cw = std::mem::take(&mut buffer.char_widths);
                let h = buffer.user_h_scale;
                for w in &mut cw {
                    *w *= h;
                }
                cw
            },
            char_x_offsets: Vec::new(),
            heading_level: None,
            rotation_degrees: buffer.rotation_degrees,
            wmode: buffer.wmode,
            text_rise: buffer.text_rise,
            rtl_draw_logical: false,
        };
        self.span_sequence_counter += 1;

        if !self.is_content_suppressed() {
            self.spans.push(span);
        }
        Ok(())
    }

    /// Calculate total width of TJ buffer using PDF spec formula.
    ///
    /// Process TJ array according to configured word boundary detection mode.
    ///
    /// Per PDF Spec ISO 32000-1:2008 Section 9.4.4,
    /// this method dispatches to either:
    /// - process_tj_array_tiebreaker(): WordBoundaryMode::Tiebreaker (default)
    /// - process_tj_array_primary(): WordBoundaryMode::Primary
    pub(super) fn process_tj_array(&mut self, array: &[TextElement]) -> Result<()> {
        match self.word_boundary_mode {
            WordBoundaryMode::Tiebreaker => self.process_tj_array_tiebreaker(array),
            WordBoundaryMode::Primary => self.process_tj_array_primary(array),
        }
    }

    /// Process TJ array using tiebreaker mode (backward compatible).
    ///
    /// This is the legacy code path used when
    /// WordBoundaryMode::Tiebreaker is configured.
    ///
    /// Maintains 100% backward compatibility with existing behavior.
    /// Word boundaries are detected only as a tiebreaker when TJ offset
    /// and geometric signals contradict each other.
    ///
    /// Per PDF Spec ISO 32000-1:2008, Section 9.4.4 NOTE 6:
    /// "The performance of text searching (and other text extraction operations) is
    /// significantly better if the text strings are as long as possible."
    ///
    /// This method buffers consecutive strings into a single span, only breaking on:
    /// - Large negative offsets (indicating word boundaries)
    /// - End of TJ array
    pub(super) fn process_tj_array_tiebreaker(&mut self, array: &[TextElement]) -> Result<()> {
        // Character-level tracking for word boundary detection
        // Collect detailed character information during TJ array processing
        // Per ISO 32000-1:2008 Section 9.4.4, character-level data improves accuracy

        self.tj_character_array.clear();
        self.current_x_position = 0.0;

        // Copy state data to avoid holding reference while borrowing self mutably
        let font_size = self.state_stack.current().font_size;
        let horizontal_scaling = self.state_stack.current().horizontal_scaling / 100.0;
        let font_name = self.state_stack.current().font_name.clone();
        let char_space = self.state_stack.current().char_space;
        let word_space = self.state_stack.current().word_space;

        let mut buffer = TjBuffer::new(
            self.state_stack.current(),
            self.current_mcid,
            self.cached_current_font.clone(),
        );
        let mut _element_count = 0;

        for (idx, element) in array.iter().enumerate() {
            _element_count += 1;
            match element {
                TextElement::String(s) => {
                    // Collect character-level data before processing buffer
                    // Extract individual characters with their properties
                    if let Some(ref name) = font_name {
                        if let Some(font) = self.fonts.get(name) {
                            // Process each byte in the string
                            for &byte in s.iter() {
                                // Normalize character code through encoding.
                                // This ensures word boundary detection works on actual characters,
                                // not raw byte codes from custom encodings
                                let char_code = font
                                    .get_encoded_char(byte)
                                    .map(|ch| ch as u32)
                                    .unwrap_or(byte as u32);

                                let glyph_width = font.get_glyph_width(byte as u16);

                                // Check if this is a ligature character (U+FB00-U+FB04)
                                let is_ligature = Self::is_ligature_code(char_code);

                                // Create CharacterInfo for this character
                                // The tj_offset will be applied when we encounter the next Offset element
                                let char_info = CharacterInfo {
                                    code: char_code,
                                    glyph_id: None, // Could be enhanced to extract actual GID
                                    width: glyph_width,
                                    x_position: self.current_x_position,
                                    tj_offset: None, // Will be set if next element is Offset
                                    font_size,
                                    is_ligature,
                                    original_ligature: None,
                                    protected_from_split: false,
                                };

                                self.tj_character_array.push(char_info);

                                // Update current X position (in text space units)
                                // Per PDF Spec: account for character spacing and scaling
                                let char_advance = glyph_width * horizontal_scaling
                                    + char_space
                                    + (if byte == 0x20 { word_space } else { 0.0 });
                                self.current_x_position += char_advance;
                            }
                        }
                    }

                    // Single-pass: append unicode + compute width + advance position
                    self.append_advance_buffer(&mut buffer, s)?;
                }
                TextElement::Offset(offset) => {
                    // Track TJ offset for statistical analysis
                    // Per ISO 32000-1:2008 Section 9.4.4, collect all TJ values
                    // to detect justified vs normal text through coefficient of variation
                    if self.tj_offset_history.len() < 10000 {
                        // Keep history reasonable size (first 10k offsets per document)
                        // and update the running accumulators.
                        let x = *offset as f64;
                        self.tj_sum += x;
                        self.tj_sum_sq += x * x;
                        self.tj_offset_history.push(*offset);
                        self.tj_stats_len = self.tj_offset_history.len();
                    }

                    // Associate TJ offset with the last character
                    // The offset applies AFTER the previous string, affecting spacing to next string
                    if !self.tj_character_array.is_empty() {
                        let last_idx = self.tj_character_array.len() - 1;
                        self.tj_character_array[last_idx].tj_offset = Some(*offset as i32);
                    }

                    // Check if this offset indicates a word boundary
                    // Per PDF spec: negative offsets increase spacing
                    // Use geometry-based adaptive threshold
                    let threshold = self.calculate_adaptive_tj_threshold();
                    if *offset < threshold {
                        // Note: #365 split-word symptoms ("diffe rent", "cha nge",
                        // "equivalen t") are handled at the higher level by the
                        // intra-word kerning guard in `should_insert_space`. An
                        // earlier TJ-side guard here (commit b2c6484) used a
                        // letter-letter + |offset| < space-glyph-width rule, but
                        // that rule misclassified real inter-word gaps in
                        // tightly-justified PDFs (LaTeX academic papers, Docling
                        // output) where producers encode word boundaries as TJ
                        // offsets smaller than a full space glyph. The
                        // span-merge-time guard has more context (full bbox,
                        // WordBoundaryDetector) and avoids that false positive.
                        //
                        // Check if buffer ends with space BEFORE flushing
                        // This prevents double spaces when TJ processor inserts space
                        // AND span merging would insert space at the same boundary.
                        let buffer_ends_with_space = !buffer.unicode.is_empty()
                            && buffer
                                .unicode
                                .chars()
                                .next_back()
                                .map(|c| c.is_whitespace())
                                .unwrap_or(false);

                        // Flush buffer before space
                        self.flush_tj_buffer(buffer)?;

                        // Check if the next element in the TJ array is a string
                        // that starts with whitespace. If so, DON'T insert a space to avoid doubling.
                        // This prevents patterns like "word " + " next" = "word next" (double space)
                        let next_element_starts_with_space = if idx + 1 < array.len() {
                            if let TextElement::String(next_s) = &array[idx + 1] {
                                next_s.first().is_some_and(|&byte| {
                                    byte == 0x20 || byte == 0x09 || byte == 0x0A || byte == 0x0D
                                })
                            } else {
                                false
                            }
                        } else {
                            false
                        };

                        // Only insert space if neither side already has whitespace
                        if !buffer_ends_with_space && !next_element_starts_with_space {
                            // Insert space character as separate span
                            self.insert_space_as_span()?;
                        }

                        // Apply the TJ offset to the text matrix BEFORE
                        // creating the new buffer so its `user_pos_x`
                        // captures the actual draw position of the next
                        // string. Otherwise the buffer anchors at the
                        // pre-offset position and every subsequent span
                        // on the line inherits the missing tx.
                        self.advance_position_for_offset(*offset)?;

                        // Start new buffer with current state
                        buffer = TjBuffer::new(
                            self.state_stack.current(),
                            self.current_mcid,
                            self.cached_current_font.clone(),
                        );
                    } else {
                        // Sub-threshold offset: matrix advances but the
                        // current buffer keeps accumulating, so apply
                        // the offset unconditionally here as well.
                        self.advance_position_for_offset(*offset)?;
                        // Fold the same displacement into the buffer's
                        // advance record. Historically only the text matrix
                        // moved, so these kerning/word-space offsets were
                        // dropped from `char_widths`/`accumulated_width` —
                        // leaving the span's reconstructed per-glyph positions
                        // drifting behind the true render (poppler/PDFium/
                        // pymupdf all fold the offset into the advance). On
                        // justified body text drawn as one continuous buffer,
                        // the many small post-space offsets accumulate into a
                        // multi-point undershoot. Folding keeps
                        // `sum(char_widths) == accumulated_width == matrix
                        // advance` by construction.
                        self.fold_offset_into_buffer(&mut buffer, *offset);
                    }
                }
            }
        }

        // Flush remaining buffer
        if !buffer.is_empty() {
            self.flush_tj_buffer(buffer)?;
        }

        Ok(())
    }

    /// Process TJ array using primary detection mode.
    ///
    /// This implementation:
    /// 1. Creates BoundaryContext from graphics state
    /// 2. Calls WordBoundaryDetector to detect boundaries in tj_character_array
    /// 3. Apply ligature expansion decisions
    /// 4. Partitions characters into clusters at boundary positions
    /// 5. Converts each cluster to a TextSpan with proper bounding boxes
    /// 6. Marks spans with primary_detected flag
    pub(super) fn process_tj_array_primary(&mut self, array: &[TextElement]) -> Result<()> {
        // Primary detection mode implementation

        // Step 1: If no characters collected, fall back to tiebreaker behavior
        if self.tj_character_array.is_empty() {
            return self.process_tj_array_tiebreaker(array);
        }

        // Mark pattern contexts BEFORE boundary detection
        // This protects email and URL patterns from being split at word boundaries
        let pattern_config = crate::extractors::PatternPreservationConfig::default();
        crate::extractors::PatternDetector::mark_pattern_contexts(
            &mut self.tj_character_array,
            &pattern_config,
        )?;

        // Step 2: Create BoundaryContext from current graphics state
        let context = self.create_boundary_context();

        // Step 3: Create WordBoundaryDetector and detect boundaries
        // OPTIMIZATION: Detect document script profile to skip unnecessary detectors (Issue #1 fix)
        let script = DocumentScript::detect_from_characters(&self.tj_character_array);
        let detector = WordBoundaryDetector::new().with_document_script(script);
        let boundaries = detector.detect_word_boundaries(&self.tj_character_array, &context);

        // Step 4: If no boundaries detected, process entire array as single span
        if boundaries.is_empty() {
            // All characters form a single word
            return self.process_tj_array_tiebreaker(array);
        }

        // Step 3.5: Apply ligature expansion decisions
        // This intelligently splits ligatures at word boundaries
        self.apply_ligature_decisions()?;

        // Step 5: Partition characters into clusters at boundary positions
        let clusters =
            self.partition_characters_by_boundaries(&self.tj_character_array, boundaries);

        // Step 6: Convert each cluster to a TextSpan
        for cluster in clusters {
            if !cluster.is_empty() {
                self.cluster_to_span(&cluster)?;
            }
        }

        Ok(())
    }

    /// Create BoundaryContext from current graphics state.
    ///
    /// Per ISO 32000-1:2008 Section 9.3, extracts text state parameters
    /// used by WordBoundaryDetector to make boundary decisions.
    pub(super) fn create_boundary_context(&self) -> BoundaryContext {
        let state = self.state_stack.current();
        BoundaryContext {
            font_size: state.font_size,
            horizontal_scaling: state.horizontal_scaling,
            word_spacing: state.word_space,
            char_spacing: state.char_space,
        }
    }

    /// Partition character array into clusters at boundary positions.
    ///
    /// # Arguments
    /// * `characters` - Full character array from TJ processing
    /// * `boundaries` - Boundary indices (positions where word boundaries occur)
    ///
    /// # Returns
    /// Vector of character clusters, where boundaries separate clusters
    pub(super) fn partition_characters_by_boundaries(
        &self,
        characters: &[CharacterInfo],
        boundaries: Vec<usize>,
    ) -> Vec<Vec<CharacterInfo>> {
        if boundaries.is_empty() {
            return vec![characters.to_vec()];
        }

        let mut clusters = Vec::new();
        let mut prev = 0;

        for boundary_idx in boundaries {
            if boundary_idx > prev {
                clusters.push(characters[prev..boundary_idx].to_vec());
            }
            prev = boundary_idx;
        }

        // Add remaining characters after last boundary
        if prev < characters.len() {
            clusters.push(characters[prev..].to_vec());
        }

        clusters
    }

    /// Convert a character cluster to a TextSpan.
    ///
    /// Calculates bounding box from character positions and creates
    /// a single TextSpan marked with primary_detected flag.
    ///
    /// # Arguments
    /// * `cluster` - Character cluster from partitioning
    pub(super) fn cluster_to_span(&mut self, cluster: &[CharacterInfo]) -> Result<()> {
        if cluster.is_empty() {
            return Ok(());
        }

        // Snapshot the current MCID scope before borrowing graphics
        // state so the borrow checker doesn't reject the
        // `current_mcid_scope()` call at span construction time.
        let mcid_scope = self.current_mcid_scope();
        let state = self.state_stack.current();

        // Step 1: Calculate bounding box from character positions in text space
        // X position: from first character to end of last character
        let text_min_x = cluster[0].x_position;
        // Safety: caller checks cluster.is_empty() above and returns early
        let last = cluster.last().expect("cluster verified non-empty above");
        let text_max_x = last.x_position + last.width;
        let text_width = (text_max_x - text_min_x).max(0.0);

        // Height from font size
        let height = cluster[0].font_size.abs() * state.text_matrix.d.abs().max(1.0);

        // Step 2: Apply CTM to convert from text space to user space
        // Per PDF Spec ISO 32000-1:2008 Section 9.4.4
        let text_matrix = state.text_matrix;
        let ctm = state.ctm;
        let text_pos = text_matrix.transform_point(text_min_x, 0.0);
        let user_pos = ctm.transform_point(text_pos.x, text_pos.y);

        // Transform the width as well (accounting for matrix scaling)
        let user_width = text_width * text_matrix.a.abs() * ctm.a.abs();

        // Step 3: Create bounding box rectangle in user space
        let bbox = Rect {
            x: user_pos.x,
            y: user_pos.y,
            width: user_width.max(text_width), // Use larger of the two for safety
            height,
        };

        // Step 3: Convert characters to Unicode string
        // Use same decoding as existing code
        let mut unicode_text = if let Some(font_name) = state.font_name.as_ref() {
            if let Some(font) = self.fonts.get(font_name) {
                let mut text = String::new();
                for char_info in cluster {
                    if let Some(decoded) = font.char_to_unicode(char_info.code) {
                        text.push_str(&decoded);
                    }
                }
                text
            } else {
                String::new()
            }
        } else {
            String::new()
        };

        // Step 3b: RTL text correction — reverse visual-order characters to logical order.
        //
        // PDF stores characters in content-stream order. For RTL scripts
        // (Arabic / Hebrew), the producer may emit text in either:
        //   * **visual order** — glyphs drawn left-to-right in user space
        //     even though the script reads right-to-left (legacy Acrobat
        //     output, pre-shaped Arabic, the Magic Palace Eilat PDF
        //     from issue #537), OR
        //   * **logical order** — glyphs drawn right-to-left in user space
        //     because the producer ran its own bidi pass before drawing
        //     (modern Word with bidi, the pdfium `hebrew_mirrored.pdf`
        //     test fixture).
        //
        // We use the confidence-gated geometric detector
        // [`text::bidi::detect_visual_order_run`] (v0.3.54 #537) when the
        // cluster has ≥4 RTL letters with clear X-monotonicity. For
        // shorter clusters (or `Ambiguous` verdict) we fall back to the
        // pre-v0.3.54 simple `last_x > first_x` heuristic — keeps the
        // existing 2-3-char RTL run behaviour byte-identical so the
        // upstream invariants (Arabic CID-TrueType samples, the
        // `right_to_left_02` fixture) still pass.
        if unicode_text.len() > 1 && cluster.len() >= 2 {
            let has_rtl = unicode_text
                .chars()
                .any(|c| crate::text::rtl_detector::is_rtl_text(c as u32));
            if has_rtl {
                // Build (char, user_x) pairs for the geometric detector.
                // One pair per source character — when the decoded
                // string has more chars than the cluster (e.g. ligature
                // expansion `fi` → "fi"), use the first decoded char as
                // a representative since they share the same source x.
                let font_for_cluster = state.font_name.as_ref().and_then(|n| self.fonts.get(n));
                let mut chars_with_x: Vec<(char, f32)> = Vec::with_capacity(cluster.len());
                for ci in cluster {
                    let decoded_first = font_for_cluster
                        .and_then(|f| f.char_to_unicode(ci.code))
                        .and_then(|s| s.chars().next());
                    if let Some(c) = decoded_first {
                        let p = text_matrix.transform_point(ci.x_position, 0.0);
                        let user_x = ctm.transform_point(p.x, p.y).x;
                        chars_with_x.push((c, user_x));
                    }
                }
                let verdict = crate::text::bidi::detect_visual_order_run(&chars_with_x);
                // Pre-v0.3.54 simple heuristic — used only as the
                // `Ambiguous` fallback (short cluster or mixed signal) so
                // existing 2-3-char RTL runs keep working; the pdfium
                // `hebrew_mirrored.pdf` fixture and similar land on
                // `Logical` above and are left alone regardless.
                let first_x = {
                    let p = text_matrix.transform_point(cluster[0].x_position, 0.0);
                    ctm.transform_point(p.x, p.y).x
                };
                let last_x = {
                    let p = text_matrix.transform_point(last.x_position, 0.0);
                    ctm.transform_point(p.x, p.y).x
                };
                unicode_text = crate::text::bidi::apply_rtl_verdict(
                    &unicode_text,
                    verdict,
                    last_x > first_x,
                    matches!(state.render_mode, 3 | 7),
                );
            }
        }

        // Step 4: Determine font weight
        let font_weight = if let Some(font_name) = state.font_name.as_ref() {
            if let Some(font) = self.fonts.get(font_name) {
                if font.is_bold() {
                    FontWeight::Bold
                } else {
                    FontWeight::Normal
                }
            } else {
                FontWeight::Normal
            }
        } else {
            FontWeight::Normal
        };

        // Determine if italic
        let is_italic = state
            .font_name
            .as_ref()
            .and_then(|name| self.fonts.get(name))
            .map(|font| font.is_italic())
            .unwrap_or(false);

        // Step 5: Create TextSpan with primary_detected flag
        let span = TextSpan {
            provenance: None,
            text: unicode_text,
            bbox,
            font_name: state
                .font_name
                .clone()
                .unwrap_or_else(|| "Unknown".to_string()),
            font_size: cluster[0].font_size,
            font_weight,
            color: Color::new(
                state.fill_color_rgb.0,
                state.fill_color_rgb.1,
                state.fill_color_rgb.2,
            ),
            mcid: self.current_mcid,
            mcid_scope: Some(mcid_scope),
            sequence: self.span_sequence_counter,
            split_boundary_before: false,
            offset_semantic: false,
            char_spacing: state.char_space,
            word_spacing: state.word_space,
            horizontal_scaling: state.horizontal_scaling,
            is_italic,
            is_monospace: false,
            primary_detected: true,
            artifact_type: None,
            char_widths: vec![],
            char_x_offsets: Vec::new(),
            heading_level: None,
            rotation_degrees: snap_run_rotation(&state.ctm.multiply(&state.text_matrix)),
            wmode: state.text_wmode,
            text_rise: if state.font_size > 0.0 {
                state.text_rise / state.font_size
            } else {
                0.0
            },
            rtl_draw_logical: false,
        };

        // Step 6: Increment sequence counter and add to spans
        self.span_sequence_counter += 1;
        if !self.is_content_suppressed() {
            self.spans.push(span);
        }

        Ok(())
    }
}
