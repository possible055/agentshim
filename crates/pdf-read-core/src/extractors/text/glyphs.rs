use super::*;

impl<'doc> TextExtractor<'doc> {
    /// Check if a character code is a ligature (U+FB00-U+FB04).
    ///
    /// Standard ligatures supported:
    /// - U+FB00: ff (LATIN SMALL LIGATURE FF)
    /// - U+FB01: fi (LATIN SMALL LIGATURE FI)
    /// - U+FB02: fl (LATIN SMALL LIGATURE FL)
    /// - U+FB03: ffi (LATIN SMALL LIGATURE FFI)
    /// - U+FB04: ffl (LATIN SMALL LIGATURE FFL)
    pub(super) fn is_ligature_code(code: u32) -> bool {
        matches!(code, 0xFB00..=0xFB04)
    }

    /// Apply ligature expansion decisions after word boundary detection.
    ///
    /// This method processes the character array after boundary detection,
    /// making intelligent decisions about whether to split ligatures.
    ///
    /// Algorithm:
    /// 1. Iterate through character array
    /// 2. For each ligature character:
    ///    - Get next character (if exists)
    ///    - Call LigatureDecisionMaker::decide()
    ///    - If Split: expand to component characters with proportional widths
    ///    - If Keep: leave as-is
    /// 3. Recalculate x_positions for all following characters after splits
    pub(super) fn apply_ligature_decisions(&mut self) -> Result<()> {
        use crate::text::ligature_processor::{
            expand_ligature_to_chars, LigatureDecision, LigatureDecisionMaker,
        };

        let context = self.create_boundary_context();
        let mut result = Vec::new();
        let mut i = 0;

        // OPTIMIZATION: Single-pass reconstruction instead of Vec::insert() in loop
        // This fixes O(n²) complexity to O(n) by avoiding repeated insertions
        // Issue #2 fix: Vec::insert was causing 50× slowdown for ligature-heavy PDFs
        while i < self.tj_character_array.len() {
            let char_info = &self.tj_character_array[i];

            // If not a ligature, keep as-is
            if !char_info.is_ligature {
                result.push(char_info.clone());
                i += 1;
                continue;
            }

            // Get next character without cloning (Issue #3 fix: eliminate unnecessary clones)
            let next_char = if i + 1 < self.tj_character_array.len() {
                Some(&self.tj_character_array[i + 1])
            } else {
                None
            };

            // Make decision using references
            let decision = LigatureDecisionMaker::decide(char_info, &context, next_char);

            if decision == LigatureDecision::Split {
                // Get the ligature character from code
                let ligature_char = char::from_u32(char_info.code).unwrap_or('?');
                let original_width = char_info.width;
                let original_x = char_info.x_position;
                let font_size = char_info.font_size;

                // Expand to component characters
                let components = expand_ligature_to_chars(ligature_char, original_width);

                if !components.is_empty() {
                    // Add first component (replacing the ligature)
                    let mut x_offset = 0.0;
                    result.push(CharacterInfo {
                        code: components[0].0 as u32,
                        glyph_id: char_info.glyph_id,
                        width: components[0].1,
                        x_position: original_x,
                        tj_offset: char_info.tj_offset,
                        font_size,
                        is_ligature: false,
                        original_ligature: Some(ligature_char),
                        protected_from_split: char_info.protected_from_split,
                    });
                    x_offset += components[0].1;

                    // Add remaining components (no Vec::insert needed - just push!)
                    for (comp_char, comp_width) in components.iter().skip(1) {
                        result.push(CharacterInfo {
                            code: *comp_char as u32,
                            glyph_id: None,
                            width: *comp_width,
                            x_position: original_x + x_offset,
                            tj_offset: None,
                            font_size,
                            is_ligature: false,
                            original_ligature: Some(ligature_char),
                            protected_from_split: false,
                        });
                        x_offset += comp_width;
                    }
                } else {
                    // If expansion failed, keep original ligature
                    result.push(char_info.clone());
                }
            } else {
                // Keep ligature intact
                result.push(char_info.clone());
            }

            i += 1;
        }

        // OPTIMIZATION: Replace entire array once instead of multiple insertions
        self.tj_character_array = result;
        Ok(())
    }

    /// Advance text position for a string (used in TJ array processing).
    /// Advance the text matrix position by the width of a text string.
    /// Returns the computed width so callers can accumulate it.
    pub(super) fn advance_position_for_string(&mut self, text: &[u8]) -> Result<f32> {
        let state = self.state_stack.current();
        let font_size = state.font_size;
        let horizontal_scaling = state.horizontal_scaling;
        let char_space = state.char_space;
        let word_space = state.word_space;
        let wmode = state.text_wmode;

        let font = self.cached_current_font.as_deref();

        // Hoist loop-invariant computations (font cannot change mid-operator).
        // font_matrix_a converts glyph-space widths to text-space units.
        // Standard fonts (Type1/TrueType): font_matrix_a = 0.001.
        // Type3 with identity FontMatrix: font_matrix_a = 1.0 (no /1000 division).
        // Assumes FontMatrix[1] = 0 (no glyph-axis rotation), which holds for all
        // standard fonts and virtually all Type3 fonts encountered in practice.
        let font_matrix_a = font.map(|f| f.font_matrix_a).unwrap_or(0.001);
        let fs_factor = font_size * font_matrix_a;
        let hs_factor = horizontal_scaling / 100.0;
        let cs_hs = char_space * hs_factor;
        let ws_hs = word_space * hs_factor;

        let total_width = if let Some(font) = font {
            if font.subtype != "Type0" {
                // Fast path: use precomputed 256-entry width table (simple fonts)
                let width_table = font.get_byte_to_width_table();
                let mut w_sum = 0.0f32;
                for &byte in text {
                    let mut w = width_table[byte as usize] * fs_factor * hs_factor;
                    w += cs_hs;
                    if byte == 0x20 {
                        w += ws_hs;
                    }
                    w_sum += w;
                }
                w_sum
            } else if wmode == 0 {
                // Type0/CID font, horizontal: use TextCharIter so that the byte-width
                // (1 or 2) is determined by the font's encoding / ToUnicode CMap
                // codespace, not hardcoded to 2. Per ISO 32000-1:2008 §9.7.6.2.
                let mut w_sum = 0.0f32;
                for (cid, nbytes) in TextCharIter::new(text, Some(font)) {
                    let mut w = font.get_glyph_width(cid) * fs_factor * hs_factor;
                    w += cs_hs;
                    // Per ISO 32000-1:2008 §9.3.3: Tw applies ONLY to the
                    // single-byte character code 32, never to the byte value 32
                    // inside a multi-byte code. `TextCharIter` yields the raw
                    // code plus its byte width, so gate on a single-byte 32 — a
                    // 2-byte CID #32 (0x0020) in an Identity-H/CJK font must not
                    // take Tw (it would over-advance and mis-position the run).
                    if nbytes == 1 && cid == 32 {
                        w += ws_hs;
                    }
                    w_sum += w;
                }
                w_sum
            } else {
                // Type0/CID font, vertical (WMode 1): per-glyph displacement
                // is `w1y` (from /W2 or /DW2 default), in 1000ths-of-em.
                //
                // Per ISO 32000-1:2008 §9.4.4 the vertical formula is
                //     ty = (w1y * Tfs) + Tc + Tw
                // with NO Th factor. §9.3.4 defines Tz as the horizontal
                // glyph-stretching axis — it does not scale w1y, Tc, or
                // Tw in vertical mode.
                let mut w_sum = 0.0f32;
                for (cid, nbytes) in TextCharIter::new(text, Some(font)) {
                    let w1y = font.get_vertical_metrics(cid).w1y;
                    let mut w = w1y * fs_factor;
                    w += char_space;
                    if nbytes == 1 && cid == 32 {
                        w += word_space;
                    }
                    w_sum += w;
                }
                w_sum
            }
        } else {
            // No font: use default width
            let default_w = 500.0 * fs_factor * hs_factor + cs_hs;
            let space_w = default_w + ws_hs;
            let mut w_sum = 0.0f32;
            for &byte in text {
                w_sum += if byte == 0x20 { space_w } else { default_w };
            }
            w_sum
        };

        // Update text matrix position per ISO 32000-1:2008 §9.4.4. The
        // axis-swap (horizontal vs vertical) is encapsulated in
        // GraphicsState::advance_text_matrix so this site does not branch.
        self.state_stack
            .current_mut()
            .advance_text_matrix(total_width);

        Ok(total_width)
    }

    /// Combined Unicode decode + width calculation in a single pass.
    /// Merges TjBuffer::append and advance_position_for_string for simple fonts,
    /// eliminating one full per-byte iteration per Tj operator.
    pub(super) fn append_and_advance(&mut self, text: &[u8]) -> Result<()> {
        let text = if text.len() > 32_767 {
            &text[..32_767]
        } else {
            text
        };

        let state = self.state_stack.current();
        let font_size = state.font_size;
        let horizontal_scaling = state.horizontal_scaling;
        let char_space = state.char_space;
        let word_space = state.word_space;
        let wmode = state.text_wmode;

        // Disjoint field borrows: cached_current_font (immutable) + tj_span_buffer (mutable)
        let font = self.cached_current_font.as_deref();
        // font_matrix_a converts glyph-space widths to text-space units.
        // Standard fonts (Type1/TrueType): font_matrix_a = 0.001.
        // Type3 with identity FontMatrix: font_matrix_a = 1.0 (no /1000 division).
        // Assumes FontMatrix[1] = 0 (no glyph-axis rotation), which holds for all
        // standard fonts and virtually all Type3 fonts encountered in practice.
        let font_matrix_a = font.map(|f| f.font_matrix_a).unwrap_or(0.001);
        let fs_factor = font_size * font_matrix_a;
        let hs_factor = horizontal_scaling / 100.0;
        let cs_hs = char_space * hs_factor;
        let ws_hs = word_space * hs_factor;
        // Safety: tj_span_buffer is always initialized via begin_text_object()
        let buffer = self
            .tj_span_buffer
            .as_mut()
            .expect("tj_span_buffer initialized in begin_text_object");

        let total_width = if let Some(font) = font {
            if font.subtype != "Type0" {
                // #317: UTF-8-in-simple-font detection (same heuristic as
                // `append_advance_buffer`). Some producers emit raw UTF-8
                // bytes inside PDF string literals when the font declares
                // only a Latin encoding and no ToUnicode CMap. Byte-by-byte
                // Latin decoding produces mojibake. When the slice is valid
                // UTF-8 with at least one non-Latin-1 codepoint, decode as
                // UTF-8 so non-Latin scripts (Cyrillic, Greek, CJK, …) come
                // through as their intended codepoints.
                if font.to_unicode.is_none() && text.len() >= 2 {
                    let has_high = text.iter().any(|&b| b >= 0x80);
                    if has_high {
                        if let Ok(decoded) = std::str::from_utf8(text) {
                            if decoded.chars().any(|c| c as u32 > 0xFF) {
                                let width_table = font.get_byte_to_width_table();
                                let mut w_sum = 0.0f32;
                                for &byte in text {
                                    let mut w = width_table[byte as usize] * fs_factor * hs_factor;
                                    w += cs_hs;
                                    if byte == 0x20 {
                                        w += ws_hs;
                                    }
                                    w_sum += w;
                                }
                                let char_count = decoded.chars().count();
                                if char_count > 0 {
                                    let per_char = w_sum / char_count as f32;
                                    for ch in decoded.chars() {
                                        buffer.unicode.push(ch);
                                        buffer.char_widths.push(per_char);
                                    }
                                }
                                // Fall through to the matrix update at the
                                // bottom of the function via `w_sum`. Vertical
                                // mode flips the axis inside the helper.
                                self.state_stack.current_mut().advance_text_matrix(w_sum);
                                return Ok(());
                            }
                        }
                    }
                }

                // Fast path: single pass over bytes for both Unicode and width
                let char_table = font.get_byte_to_char_table();
                let width_table = font.get_byte_to_width_table();
                let mut w_sum = 0.0f32;
                for &byte in text {
                    // Unicode decode — count chars added for per-char width tracking
                    let len_before = buffer.unicode.len();
                    let c = char_table[byte as usize];
                    if c != '\0' {
                        buffer.unicode.push(c);
                    } else {
                        // Rare: multi-char mapping or unmapped byte
                        if let Some(s) = font.char_to_unicode(byte as u32) {
                            if s != "\u{FFFD}" || preserve_unmapped_glyphs() {
                                for ch in s.chars() {
                                    if ch >= '\x20' || ch == '\t' || ch == '\n' || ch == '\r' {
                                        buffer.unicode.push(ch);
                                    }
                                }
                            }
                        } else {
                            let fb = fallback_char_to_unicode(byte as u32);
                            if fb != "\u{FFFD}" || preserve_unmapped_glyphs() {
                                for ch in fb.chars() {
                                    if ch >= '\x20' || ch == '\t' || ch == '\n' || ch == '\r' {
                                        buffer.unicode.push(ch);
                                    }
                                }
                            }
                        }
                    }
                    // Width calculation
                    let mut w = width_table[byte as usize] * fs_factor * hs_factor;
                    w += cs_hs;
                    if byte == 0x20 {
                        w += ws_hs;
                    }
                    w_sum += w;
                    // Track per-character advance widths
                    let chars_added = buffer.unicode.len() - len_before;
                    if chars_added == 1 {
                        buffer.char_widths.push(w);
                    } else if chars_added > 1 {
                        let per_char = w / chars_added as f32;
                        for _ in 0..chars_added {
                            buffer.char_widths.push(per_char);
                        }
                    }
                }
                w_sum
            } else if wmode == 0 {
                // Type0/CID font, horizontal: unified iterator handles 1- or
                // 2-byte codes per ToUnicode codespace.
                buffer.append(text)?;
                let mut w_sum = 0.0f32;
                for (char_code, _) in TextCharIter::new(text, Some(font)) {
                    let mut w = font.get_glyph_width(char_code) * fs_factor * hs_factor;
                    w += cs_hs;
                    // Standard PDF space character (code 32) triggers word spacing
                    if char_code == 32 {
                        w += ws_hs;
                    }
                    w_sum += w;
                    buffer.char_widths.push(w);
                }
                w_sum
            } else {
                // Type0/CID font, vertical (WMode 1): per-glyph displacement
                // is `w1y` (from /W2 or /DW2), in 1000ths-of-em.
                //
                // Per ISO 32000-1:2008 §9.4.4: `ty = (w1y * Tfs) + Tc + Tw`,
                // with no Th (Tz only stretches glyphs along the horizontal
                // axis per §9.3.4).
                buffer.append(text)?;
                let mut w_sum = 0.0f32;
                for (char_code, _) in TextCharIter::new(text, Some(font)) {
                    let w1y = font.get_vertical_metrics(char_code).w1y;
                    let mut w = w1y * fs_factor;
                    w += char_space;
                    if char_code == 32 {
                        w += word_space;
                    }
                    w_sum += w;
                    buffer.char_widths.push(w);
                }
                w_sum
            }
        } else {
            // No font: decode as ASCII + use default widths
            buffer.append(text)?;
            let default_w = 500.0 * fs_factor * hs_factor + cs_hs;
            let space_w = default_w + ws_hs;
            let mut w_sum = 0.0f32;
            for &byte in text {
                let w = if byte == 0x20 { space_w } else { default_w };
                w_sum += w;
                buffer.char_widths.push(w);
            }
            w_sum
        };

        buffer.accumulated_width += total_width;

        // Update text matrix position per ISO 32000-1:2008 §9.4.4. The
        // axis-swap (H vs V) is encapsulated in advance_text_matrix.
        self.state_stack
            .current_mut()
            .advance_text_matrix(total_width);

        Ok(())
    }

    /// Combined Unicode decode + width + position advance for a local buffer.
    /// Same as append_and_advance but works on an explicit buffer parameter
    /// instead of self.tj_span_buffer. Used by TJ array processing.
    pub(super) fn append_advance_buffer(
        &mut self,
        buffer: &mut TjBuffer,
        text: &[u8],
    ) -> Result<()> {
        let text = if text.len() > 32_767 {
            &text[..32_767]
        } else {
            text
        };

        let state = self.state_stack.current();
        let font_size = state.font_size;
        let horizontal_scaling = state.horizontal_scaling;
        let char_space = state.char_space;
        let word_space = state.word_space;
        let wmode = state.text_wmode;

        let font = self.cached_current_font.as_deref();
        // font_matrix_a converts glyph-space widths to text-space units.
        // Standard fonts (Type1/TrueType): font_matrix_a = 0.001.
        // Type3 with identity FontMatrix: font_matrix_a = 1.0 (no /1000 division).
        // Assumes FontMatrix[1] = 0 (no glyph-axis rotation), which holds for all
        // standard fonts and virtually all Type3 fonts encountered in practice.
        let font_matrix_a = font.map(|f| f.font_matrix_a).unwrap_or(0.001);
        let fs_factor = font_size * font_matrix_a;
        let hs_factor = horizontal_scaling / 100.0;
        let cs_hs = char_space * hs_factor;
        let ws_hs = word_space * hs_factor;

        let total_width = if let Some(font) = font {
            if font.subtype != "Type0" {
                // #317: UTF-8-in-simple-font detection.
                //
                // Some producers (Russian CAD exporters, MS Office via
                // non-English locales) emit UTF-8 byte sequences inside PDF
                // string literals for a font that only declares a Latin
                // encoding (WinAnsi, StandardEncoding, MacRoman) and no
                // ToUnicode CMap. Byte-by-byte decoding through the Latin
                // encoding produces mojibake like `ÐÐ¸ÑÑ` for "Лист".
                //
                // Heuristic: when the font has no ToUnicode and the entire
                // text slice is a valid UTF-8 sequence whose decoded
                // codepoints contain at least one non-Latin-1 character
                // (U+0100 and above), treat the slice as UTF-8 directly.
                // The non-Latin-1 gate prevents mis-interpreting genuine
                // Latin-1 Supplement content (`Résumé`, etc.) — those
                // decode entirely into U+0000..U+00FF and are left alone.
                let utf8_width: Option<f32> = if font.to_unicode.is_none() && text.len() >= 2 {
                    let has_high = text.iter().any(|&b| b >= 0x80);
                    if has_high {
                        if let Ok(decoded) = std::str::from_utf8(text) {
                            let has_non_latin1 = decoded.chars().any(|c| c as u32 > 0xFF);
                            if has_non_latin1 {
                                let width_table = font.get_byte_to_width_table();
                                let mut w_sum = 0.0f32;
                                for &byte in text {
                                    let mut w = width_table[byte as usize] * fs_factor * hs_factor;
                                    w += cs_hs;
                                    if byte == 0x20 {
                                        w += ws_hs;
                                    }
                                    w_sum += w;
                                }
                                let char_count = decoded.chars().count();
                                if char_count > 0 {
                                    let per_char = w_sum / char_count as f32;
                                    for ch in decoded.chars() {
                                        buffer.unicode.push(ch);
                                        buffer.char_widths.push(per_char);
                                    }
                                }
                                log::debug!(
                                    "UTF-8 mojibake repair: decoded {} Latin-1 bytes as {} chars via UTF-8 in font '{}'",
                                    text.len(),
                                    char_count,
                                    font.base_font
                                );
                                Some(w_sum)
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                };
                if let Some(w) = utf8_width {
                    buffer.accumulated_width += w;
                    self.state_stack.current_mut().advance_text_matrix(w);
                    return Ok(());
                }

                let char_table = font.get_byte_to_char_table();
                let width_table = font.get_byte_to_width_table();
                let mut w_sum = 0.0f32;
                for &byte in text {
                    let len_before = buffer.unicode.len();
                    let c = char_table[byte as usize];
                    if c != '\0' {
                        buffer.unicode.push(c);
                    } else if let Some(s) = font.char_to_unicode(byte as u32) {
                        if s != "\u{FFFD}" || preserve_unmapped_glyphs() {
                            for ch in s.chars() {
                                if ch >= '\x20' || ch == '\t' || ch == '\n' || ch == '\r' {
                                    buffer.unicode.push(ch);
                                }
                            }
                        }
                    } else {
                        let fb = fallback_char_to_unicode(byte as u32);
                        if fb != "\u{FFFD}" || preserve_unmapped_glyphs() {
                            for ch in fb.chars() {
                                if ch >= '\x20' || ch == '\t' || ch == '\n' || ch == '\r' {
                                    buffer.unicode.push(ch);
                                }
                            }
                        }
                    }
                    let mut w = width_table[byte as usize] * fs_factor * hs_factor;
                    w += cs_hs;
                    if byte == 0x20 {
                        w += ws_hs;
                    }
                    w_sum += w;
                    let chars_added = buffer.unicode.len() - len_before;
                    if chars_added == 1 {
                        buffer.char_widths.push(w);
                    } else if chars_added > 1 {
                        let per_char = w / chars_added as f32;
                        for _ in 0..chars_added {
                            buffer.char_widths.push(per_char);
                        }
                    }
                }
                w_sum
            } else if wmode == 0 {
                buffer.append(text)?;
                // Width calculation: use TextCharIter so byte-width respects the
                // CMap codespace (1 or 2 bytes per character). Fixes CJK fonts
                // whose encoding name doesn't match the well-known Identity-H/EUC/…
                // keyword patterns but whose ToUnicode CMap declares a 2-byte
                // codespace range (§9.7.5).
                let mut w_sum = 0.0f32;
                for (cid, nbytes) in TextCharIter::new(text, Some(font)) {
                    let mut w = font.get_glyph_width(cid) * fs_factor * hs_factor;
                    w += cs_hs;
                    if nbytes == 1 && cid == 32 {
                        w += ws_hs;
                    }
                    w_sum += w;
                    buffer.char_widths.push(w);
                }
                w_sum
            } else {
                // Type0/CID font, vertical mode: per-glyph displacement is
                // /W2 `w1y` (or /DW2 default), in 1000ths-of-em. The
                // vertical formula `ty = (w1y * Tfs) + Tc + Tw` (§9.4.4)
                // does NOT apply Th — Tz only scales glyphs horizontally
                // (§9.3.4).
                buffer.append(text)?;
                let mut w_sum = 0.0f32;
                for (cid, nbytes) in TextCharIter::new(text, Some(font)) {
                    let w1y = font.get_vertical_metrics(cid).w1y;
                    let mut w = w1y * fs_factor;
                    w += char_space;
                    if nbytes == 1 && cid == 32 {
                        w += word_space;
                    }
                    w_sum += w;
                    buffer.char_widths.push(w);
                }
                w_sum
            }
        } else {
            buffer.append(text)?;
            let default_w = 500.0 * fs_factor * hs_factor + cs_hs;
            let space_w = default_w + ws_hs;
            let mut w_sum = 0.0f32;
            for &byte in text {
                let w = if byte == 0x20 { space_w } else { default_w };
                w_sum += w;
                buffer.char_widths.push(w);
            }
            w_sum
        };

        buffer.accumulated_width += total_width;

        self.state_stack
            .current_mut()
            .advance_text_matrix(total_width);

        Ok(())
    }
}
