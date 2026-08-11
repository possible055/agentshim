use super::*;

impl<'doc> TextExtractor<'doc> {
    /// Insert a space character as a separate span.
    pub(super) fn insert_space_as_span(&mut self) -> Result<()> {
        let mcid_scope = self.current_mcid_scope();
        let state = self.state_stack.current();
        let font_size = state.font_size;
        let text_matrix = state.text_matrix;
        let ctm = state.ctm;
        let combined = ctm.multiply(&text_matrix);
        let effective_font_size =
            font_size * (combined.d * combined.d + combined.b * combined.b).sqrt();
        let word_space = state.word_space;
        let horizontal_scaling = state.horizontal_scaling;
        let wmode = state.text_wmode;

        // Calculate space displacement along the active writing axis. In
        // horizontal mode this is the glyph width (250/1000 em ≈ quarter
        // em) plus Tw, scaled by Th. In vertical mode Tz does not apply
        // (§9.3.4) and we use the same magnitude as a writing-axis step
        // — the synthetic gap a TJ offset stands in for.
        //
        // NOTE: the displacement is expressed against the raw `Tf` size,
        // not the `Tm`-scaled effective size, so for print-era producers
        // that set `/F 1 Tf` with the size in `Tm` this span is narrower
        // in device space than a quarter em. That geometry is load-bearing
        // for the downstream column/line heuristics, which were tuned
        // against it — widening it reorders text on real documents — so
        // the lockstep fix below keeps a `char_widths` entry
        // consistent with this bbox rather than rescaling both.
        let space_advance = if wmode == 0 {
            (250.0 * font_size / 1000.0 + word_space) * horizontal_scaling / 100.0
        } else {
            250.0 * font_size / 1000.0 + word_space
        };

        // Apply CTM to get position in user space
        // Per PDF Spec ISO 32000-1:2008 Section 9.4.4
        let text_pos = text_matrix.transform_point(0.0, 0.0);
        let user_pos = ctm.transform_point(text_pos.x, text_pos.y);

        log::trace!(
            "Inserting space span from TJ offset (offset_semantic=true) at position ({:.2}, {:.2})",
            user_pos.x,
            user_pos.y
        );

        let font_name_space = state
            .font_name
            .clone()
            .unwrap_or_else(|| "Unknown".to_string());
        let is_italic_space = state
            .font_name
            .as_ref()
            .and_then(|name| self.fonts.get(name))
            .map(|font| font.is_italic())
            .unwrap_or(false);
        // Bbox geometry follows the writing axis: a horizontal gap is
        // wide and font-tall; a vertical gap is glyph-em-wide and tall
        // along the writing direction. Downstream layout heuristics
        // (column detection, line breaking) read width vs height to
        // decide orientation, so labeling the synthetic-space geometry
        // correctly keeps them honest.
        let (space_width, space_height) = if wmode == 0 {
            (space_advance, effective_font_size)
        } else {
            (effective_font_size, space_advance.abs())
        };
        let span = TextSpan {
            provenance: None,
            text: " ".to_string(),
            bbox: Rect {
                x: user_pos.x,
                y: user_pos.y,
                width: space_width,
                height: space_height,
            },
            font_name: font_name_space,
            font_size: effective_font_size,
            font_weight: FontWeight::Normal,
            color: Color::new(
                state.fill_color_rgb.0,
                state.fill_color_rgb.1,
                state.fill_color_rgb.2,
            ),
            mcid: self.current_mcid,
            mcid_scope: Some(mcid_scope),
            sequence: self.span_sequence_counter,
            split_boundary_before: false,
            offset_semantic: true,
            char_spacing: state.char_space, // Tc - captured from PDF content stream
            word_spacing: state.word_space, // Tw - captured from PDF content stream
            horizontal_scaling: state.horizontal_scaling, // Tz - captured from PDF content stream
            is_italic: is_italic_space,
            is_monospace: false,
            primary_detected: false,
            artifact_type: self.current_artifact_type(),
            // One synthetic space char ⇒ one width entry, so the span-merge
            // lockstep (`char_widths.len() == text.chars().count()`) holds
            // from birth regardless of merge order. The width is
            // the bbox extent along x, consistent with `to_chars` geometry.
            char_widths: vec![space_width],
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
        self.span_sequence_counter += 1;

        log::trace!(
            "PUSH space span with offset_semantic={}",
            span.offset_semantic
        );

        if !self.is_content_suppressed() {
            self.spans.push(span);
        }

        // Do NOT advance the text matrix here. The caller drives the
        // matrix forward by the *actual* TJ offset via
        // `advance_position_for_offset` immediately after; advancing
        // by `space_width` on top of that would double-count the gap
        // and capture the wrong `user_pos_x` when the next buffer is
        // created, producing spans whose bbox.x sits ~one synthetic
        // space-width to the right of the character actually drawn.

        Ok(())
    }

    /// Advance text position for a TJ offset value.
    ///
    /// Per ISO 32000-1:2008 §9.4.4 a number element in a TJ array shifts
    /// the position along the **active** writing axis:
    ///   horizontal: tx = -offset / 1000 * font_size * Th
    ///   vertical:   ty = -offset / 1000 * font_size     (NO Th)
    /// Th (Tz) is the horizontal glyph-stretching axis (§9.3.4) and does
    /// not apply in vertical mode. The matrix-side axis-swap lives in
    /// `advance_text_matrix`.
    pub(super) fn advance_position_for_offset(&mut self, offset: f32) -> Result<()> {
        let state = self.state_stack.current();
        let font_size = state.font_size;
        let horizontal_scaling = state.horizontal_scaling;
        let wmode = state.text_wmode;

        let tx = if wmode == 0 {
            -offset / 1000.0 * font_size * horizontal_scaling / 100.0
        } else {
            -offset / 1000.0 * font_size
        };

        self.state_stack.current_mut().advance_text_matrix(tx);

        Ok(())
    }

    /// Fold a sub-threshold TJ offset into the active buffer's advance record
    /// so its `char_widths`/`accumulated_width` track the text-matrix position.
    ///
    /// The displacement is computed identically to `advance_position_for_offset`
    /// (text space, before the `user_h_scale` applied at flush) so it lands in
    /// the same units as the per-glyph advances pushed during string append.
    /// The offset conventionally belongs to the *preceding* glyph (it adjusts
    /// spacing after it), so it is added to the last recorded advance; if no
    /// glyph has been recorded yet the matrix move alone already positions the
    /// next buffer, so there is nothing to fold.
    pub(super) fn fold_offset_into_buffer(&self, buffer: &mut TjBuffer, offset: f32) {
        let Some(last) = buffer.char_widths.last_mut() else {
            return;
        };
        let state = self.state_stack.current();
        let adv = if state.text_wmode == 0 {
            -offset / 1000.0 * state.font_size * state.horizontal_scaling / 100.0
        } else {
            -offset / 1000.0 * state.font_size
        };
        *last += adv;
        buffer.accumulated_width += adv;
    }

    /// Flush accumulated Tj span buffer into a single TextSpan.
    ///
    /// This is similar to flush_tj_buffer but works with the tj_span_buffer field
    /// which accumulates consecutive Tj operators.
    pub(super) fn flush_tj_span_buffer(&mut self) -> Result<()> {
        if let Some(mut buffer) = self.tj_span_buffer.take() {
            if !buffer.is_empty() {
                // Use accumulated width from advance_position_for_string calls
                // Convert from text space to user space using pre-computed horizontal scale
                let total_width = buffer.accumulated_width * buffer.user_h_scale;

                // Use pre-computed values from buffer creation (avoids
                // matrix multiply + sqrt + HashMap lookup per flush)
                let effective_font_size = buffer.effective_font_size;
                let font_weight = buffer.font_weight;
                let is_italic_buf = buffer.is_italic;

                // Move owned strings out of buffer (avoids clone)
                let font_name_buf = buffer
                    .font_name
                    .take()
                    .unwrap_or_else(|| "Unknown".to_string());

                // #537/#826: RTL visual-order detection for the Tj-span
                // path, via the shared `apply_rtl_verdict` decision point
                // (also used by `flush_tj_buffer` and `cluster_to_span`) —
                // geometric detector when `char_widths` give us per-char x,
                // falling back to the coarse `accumulated_width > 0`
                // heuristic only when ambiguous.
                let mut text = std::mem::take(&mut buffer.unicode);
                if text.len() > 1 {
                    let has_rtl = text
                        .chars()
                        .any(|c| crate::text::rtl_detector::is_rtl_text(c as u32));
                    if has_rtl {
                        // char_widths contains text-space relative widths;
                        // reconstruct absolute user-space x by accumulating,
                        // scaling by user_h_scale and offsetting by user_pos_x.
                        let chars: Vec<char> = text.chars().collect();
                        let verdict = if chars.len() == buffer.char_widths.len()
                            && !buffer.char_widths.is_empty()
                        {
                            let mut chars_with_x: Vec<(char, f32)> =
                                Vec::with_capacity(chars.len());
                            let mut cursor_text_space = 0.0_f32;
                            for (i, c) in chars.iter().enumerate() {
                                let user_x =
                                    buffer.user_pos_x + cursor_text_space * buffer.user_h_scale;
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
                    font_name: font_name_buf,
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
                    char_spacing: 0.0, // Tc - per ISO 32000-1:2008 Section 9.3.1
                    word_spacing: 0.0, // Tw - per ISO 32000-1:2008 Section 9.3.1
                    horizontal_scaling: 100.0, // Tz - per ISO 32000-1:2008 Section 9.3.1
                    is_italic: is_italic_buf,
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

                log::trace!(
                    "FLUSH_TJ_SPAN_BUFFER creating span: text='{}', offset_semantic={} (space-only spans marked as offset_semantic)",
                    if span.text.chars().all(|c| c.is_whitespace()) {
                        "<space-only>"
                    } else {
                        crate::utils::safe_prefix(&span.text, 20)
                    },
                    span.offset_semantic
                );

                if !self.is_content_suppressed() {
                    self.spans.push(span);
                }
            }
        }
        Ok(())
    }

    pub(super) fn show_text(&mut self, text: &[u8]) -> Result<()> {
        // PDF spec Section 7.3.4.2: implementation limit of 32,767 bytes per string.
        let text = if text.len() > 32_767 {
            log::warn!(
                "String exceeds PDF spec limit: {} bytes (max 32,767), truncating",
                text.len()
            );
            &text[..32_767]
        } else {
            text
        };

        // Get current state values
        let state = self.state_stack.current();
        let font_size = state.font_size;
        let horizontal_scaling = state.horizontal_scaling;
        let char_space = state.char_space;
        let word_space = state.word_space;
        let fill_color_rgb = state.fill_color_rgb;
        let ctm = state.ctm;
        let wmode = state.text_wmode;

        // Get current font from cached reference
        let font = self.cached_current_font.as_deref();

        for (char_code, _) in TextCharIter::new(text, font) {
            // Get current text matrix (may be updated by previous characters in this string)
            let state = self.state_stack.current();
            let text_matrix = state.text_matrix;

            // Get Unicode string using font mapping
            let unicode_string = if let Some(font) = font {
                font.char_to_unicode(char_code as u32)
                    .unwrap_or_else(|| fallback_char_to_unicode(char_code as u32))
            } else if char_code < 256 && (char_code as u8).is_ascii() {
                (char_code as u8 as char).to_string()
            } else {
                "?".to_string()
            };

            // Calculate character position in user space
            let text_pos = text_matrix.transform_point(0.0, 0.0);
            let pos = ctm.transform_point(text_pos.x, text_pos.y);

            // Calculate effective font size
            let combined_char = ctm.multiply(&text_matrix);
            let effective_font_size = font_size
                * (combined_char.d * combined_char.d + combined_char.b * combined_char.b).sqrt();

            // Calculate character dimensions using accurate glyph width
            let glyph_width_font_units = if let Some(font) = font {
                font.get_glyph_width(char_code)
            } else {
                500.0 // Default 0.5em
            };

            // font_matrix_a converts glyph-space widths to text-space units.
            // Standard fonts (Type1/TrueType): font_matrix_a = 0.001.
            // Type3 with identity FontMatrix: font_matrix_a = 1.0 (no /1000 division).
            // Assumes FontMatrix[1] = 0 (no glyph-axis rotation), which holds for all
            // standard fonts and virtually all Type3 fonts encountered in practice.
            let font_matrix_a = font.map(|f| f.font_matrix_a).unwrap_or(0.001);
            let fs_factor = font_size * font_matrix_a;
            let hs_factor = horizontal_scaling / 100.0;
            let glyph_width_user_space = glyph_width_font_units * fs_factor * hs_factor;

            // Advance along the active writing axis per ISO 32000-1 §9.4.4:
            //   horizontal: tx = (w0 * Tfs + Tc + Tw) * Th
            //   vertical:   ty = w1y * Tfs + Tc + Tw    (NO Th — Tz is a
            //               glyph-stretching factor on the X axis only;
            //               see §9.3.4).
            let mut tx = if wmode == 0 {
                glyph_width_user_space
                    + char_space * hs_factor
                    + if char_code == 32 {
                        word_space * hs_factor
                    } else {
                        0.0
                    }
            } else {
                let w1y = font
                    .map(|f| f.get_vertical_metrics(char_code).w1y)
                    .unwrap_or(crate::fonts::VerticalMetrics::SPEC_DEFAULT.w1y);
                w1y * fs_factor + char_space + if char_code == 32 { word_space } else { 0.0 }
            };

            // For TextChar, we use the device-space width
            let glyph_width_device_space = glyph_width_user_space * combined_char.a.abs();
            let tx_device_space = tx * combined_char.a.abs();
            let height_device_space = effective_font_size;
            // Quiet unused-mut warning when wmode != 0 and tx is read-only after this point.
            let _ = &mut tx;

            // Determine font weight and style
            let (font_weight, is_italic_char) = if let Some(font) = font {
                (
                    if font.is_bold() {
                        FontWeight::Bold
                    } else {
                        FontWeight::Normal
                    },
                    font.is_italic(),
                )
            } else {
                (FontWeight::Normal, false)
            };

            // Get color
            let (r, g, b) = fill_color_rgb;
            let color = Color::new(r, g, b);

            // Compose CTM and text_matrix for full transformation
            let final_matrix = ctm.multiply(&text_matrix);
            let rotation_degrees = final_matrix.b.atan2(final_matrix.a).to_degrees();

            // Guard against malformed fonts
            let unicode_string = if unicode_string.chars().count() > 8 {
                unicode_string.chars().next().unwrap_or('?').to_string()
            } else {
                unicode_string
            };

            // Process each character in the expanded string (ligatures)
            let char_count = unicode_string.chars().count();
            let char_width_device = if char_count > 0 {
                glyph_width_device_space / char_count as f32
            } else {
                glyph_width_device_space
            };
            let char_width_user = if char_count > 0 {
                glyph_width_user_space / char_count as f32
            } else {
                glyph_width_user_space
            };
            // Spread the total advance evenly across the ligature's output chars.
            // Tc applies once per character *code*, not per output glyph, so this
            // approximation slightly over-distributes Tc for multi-char ligatures —
            // the same trade-off advance_width already makes for glyph_width_device.
            let rendered_advance_per_char = if char_count > 0 {
                tx_device_space / char_count as f32
            } else {
                tx_device_space
            };

            for (char_index, unicode_char) in unicode_string.chars().enumerate() {
                let should_skip = unicode_char == '\0'
                    || (unicode_char.is_control()
                        && unicode_char != '\t'
                        && unicode_char != '\n'
                        && unicode_char != '\r');

                if !should_skip {
                    let x_offset_device = char_index as f32 * char_width_device;
                    let x_offset_user = char_index as f32 * char_width_user;

                    let char_origin_x = pos.x + x_offset_device;
                    let char_origin_y = pos.y;

                    let text_char = TextChar {
                        char: unicode_char,
                        bbox: Rect::new(
                            char_origin_x,
                            char_origin_y,
                            char_width_device,
                            height_device_space,
                        ),
                        font_name: font.map(|f| f.base_font.clone()).unwrap_or_default(),
                        font_size: effective_font_size,
                        font_weight,
                        color,
                        mcid: self.current_mcid,
                        is_italic: is_italic_char,
                        is_monospace: false,
                        origin_x: char_origin_x,
                        origin_y: char_origin_y,
                        rotation_degrees,
                        advance_width: char_width_device,
                        rendered_advance: rendered_advance_per_char,
                        ascent: font.map(|f| f.ascent).unwrap_or(0.95) * effective_font_size,
                        descent: font.map(|f| f.descent).unwrap_or(-0.35) * effective_font_size,
                        matrix: Some([
                            final_matrix.a,
                            final_matrix.b,
                            final_matrix.c,
                            final_matrix.d,
                            final_matrix.e + x_offset_user,
                            final_matrix.f,
                        ]),
                    };

                    if !self.is_content_suppressed() {
                        self.chars.push(text_char);
                    }
                }
            }

            // Update text matrix per ISO 32000-1:2008 §9.4.4. The axis swap
            // (x for WMode 0, y for WMode 1) is encapsulated in
            // advance_text_matrix so this site does not branch.
            self.state_stack.current_mut().advance_text_matrix(tx);
        }

        Ok(())
    }

    /// Get the number of extracted characters.
    pub fn char_count(&self) -> usize {
        self.chars.len()
    }

    /// Clear all extracted characters.
    pub fn clear(&mut self) {
        self.chars.clear();
    }
}
