use super::*;

impl TextRasterizer {
    /// Render text using direct CID-to-GID mapping, bypassing harfrust shaping.
    /// Used for CID subset fonts that have embedded data but no usable Unicode cmap.
    /// Per PDF spec section 9.7.4, CIDToGIDMap maps CIDs to glyph indices in the TrueType font.
    pub(super) fn render_cid_direct(
        &self,
        pixmap: &mut Pixmap,
        bytes: &[u8],
        font_info: &crate::fonts::FontInfo,
        font_data: &[u8],
        index: u32,
        paint: &Paint,
        base_transform: Transform,
        gs: &GraphicsState,
        clip_mask: Option<&tiny_skia::Mask>,
    ) -> Result<f32> {
        let font_size = gs.font_size;
        let h_scale = gs.horizontal_scaling / 100.0;

        let ttf_face = ttf_parser::Face::parse(font_data, index)
            .map_err(|e| Error::InvalidPdf(format!("Failed to parse embedded font: {}", e)))?;
        let units_per_em = ttf_face.units_per_em() as f32;
        let scale = font_size / units_per_em;

        let text_transform = Transform::from_row(
            gs.text_matrix.a,
            gs.text_matrix.b,
            gs.text_matrix.c,
            gs.text_matrix.d,
            gs.text_matrix.e,
            gs.text_matrix.f,
        );
        let combined_base = base_transform.pre_concat(text_transform);

        let mut x_cursor: f32 = 0.0;
        let mut y_cursor: f32 = 0.0;
        let wmode = gs.text_wmode;

        // Iterate over character codes from the raw bytes
        for (char_code, _bytes_consumed) in TextCharIter::new(bytes, Some(font_info)) {
            // Map character code to GID based on font type:
            // - Type0 (CID-keyed) without CIDToGIDMap → CID is GID
            //   (Identity-H/Identity-V emission, the case our writer
            //   uses for CFF subsets re-embedded with a synthesised
            //   cmap). The cff_gid_map only applies when the font is
            //   a SIMPLE Type1/CFF font — i.e. `subtype != "Type0"`.
            // - CIDFontType2: CIDToGIDMap maps CID → GID.
            // - CFF simple font (Type1, non-Type0): cff_gid_map maps
            //   byte → GID.
            // - Simple TrueType: consult the embedded font's cmap
            //   directly (the PDF content byte is the cmap input
            //   under the font's declared encoding; ISO 32000-1
            //   §9.6.6.4).
            // - Default: identity mapping.
            let gid = if font_info.subtype == "Type0" {
                match &font_info.cid_to_gid_map {
                    Some(crate::fonts::CIDToGIDMap::Identity) => char_code,
                    Some(crate::fonts::CIDToGIDMap::Explicit(map)) => {
                        *map.get(char_code as usize).unwrap_or(&0)
                    }
                    None => char_code, // CIDFontType0 + Identity-H: CID == GID
                }
            } else if let Some(cff_map) = &font_info.cff_gid_map {
                *cff_map.get(&(char_code as u8)).unwrap_or(&0)
            } else if font_info.cid_to_gid_map.is_none() {
                cmap_byte_to_gid(&ttf_face, char_code as u8).unwrap_or(0)
            } else {
                match &font_info.cid_to_gid_map {
                    Some(crate::fonts::CIDToGIDMap::Identity) => char_code,
                    Some(crate::fonts::CIDToGIDMap::Explicit(map)) => {
                        *map.get(char_code as usize).unwrap_or(&0)
                    }
                    None => char_code,
                }
            };
            let cid = char_code; // For width lookup

            // Get width from PDF metrics (horizontal) and vertical advance
            // + origin offset (vertical mode). Both lookups read from
            // FontInfo's hot caches; the vertical lookup is only consulted
            // when wmode==1, keeping the horizontal fast path unchanged.
            let pdf_width = font_info.get_glyph_width(cid);
            let x_advance = pdf_width * font_size / 1000.0;
            let (y_step, paint_origin_dx, paint_origin_dy) = if wmode == 1 {
                let m = font_info.get_vertical_metrics(cid);
                (
                    m.w1y * font_size / 1000.0,
                    -m.v_x * font_size / 1000.0,
                    -m.v_y * font_size / 1000.0,
                )
            } else {
                (0.0, 0.0, 0.0)
            };

            // Get Unicode character for space/word-space detection.
            // Use '\0' as the sentinel for "no mapping" so that bytes without a
            // Unicode entry (e.g. ligatures and accented chars in symbolic TrueType
            // fonts that use the Mac Roman cmap path) are not silently treated as
            // spaces and dropped from the rendered output.
            let char_str = font_info.char_to_unicode(cid as u32).unwrap_or_default();
            let char_at_pos = char_str.chars().next().unwrap_or('\0');

            // Draw glyph outline
            if gid != 0 || char_at_pos.is_whitespace() {
                if !char_at_pos.is_whitespace() {
                    let mut pb = PathBuilder::new();
                    let mut builder = SkiaOutlineBuilder(&mut pb);
                    if ttf_face
                        .outline_glyph(ttf_parser::GlyphId(gid), &mut builder)
                        .is_some()
                    {
                        if let Some(path) = pb.finish() {
                            let (rise_x, rise_y) = if wmode == 0 {
                                (0.0, gs.text_rise)
                            } else {
                                (gs.text_rise, 0.0)
                            };
                            let px = (x_cursor + paint_origin_dx) * h_scale + rise_x;
                            let py = y_cursor + paint_origin_dy + rise_y;
                            let glyph_transform =
                                combined_base.pre_translate(px, py).pre_scale(scale, scale);
                            pixmap.fill_path(
                                &path,
                                paint,
                                tiny_skia::FillRule::Winding,
                                glyph_transform,
                                clip_mask,
                            );
                        }
                    }
                }
            }

            if wmode == 0 {
                x_cursor += x_advance + gs.char_space;
                if char_at_pos == ' ' {
                    x_cursor += gs.word_space;
                }
            } else {
                y_cursor += y_step + gs.char_space;
                if char_at_pos == ' ' {
                    y_cursor += gs.word_space;
                }
            }
        }

        Ok(if wmode == 0 { x_cursor } else { y_cursor })
    }

    /// Fallback simple rendering if no font found.
    /// Returns the total horizontal advance in PDF points.
    pub(super) fn render_text_fallback(
        &self,
        pixmap: &mut Pixmap,
        text: &str,
        paint: &Paint,
        base_transform: Transform,
        gs: &GraphicsState,
        clip_mask: Option<&tiny_skia::Mask>,
    ) -> Result<f32> {
        // Just draw rectangles for now as very last resort
        let font_size = gs.font_size;
        let char_width = font_size * 0.6;
        let mut x_cursor: f32 = 0.0;
        let h_scale = gs.horizontal_scaling / 100.0;

        let text_transform = Transform::from_row(
            gs.text_matrix.a,
            gs.text_matrix.b,
            gs.text_matrix.c,
            gs.text_matrix.d,
            gs.text_matrix.e,
            gs.text_matrix.f,
        );
        let transform = base_transform.pre_concat(text_transform);

        for c in text.chars() {
            if !c.is_whitespace() {
                let mut pb = PathBuilder::new();
                if let Some(rect) = tiny_skia::Rect::from_xywh(
                    x_cursor * h_scale,
                    0.0,
                    char_width * 0.8,
                    font_size * 0.8,
                ) {
                    pb.push_rect(rect);
                    if let Some(path) = pb.finish() {
                        pixmap.fill_path(
                            &path,
                            paint,
                            tiny_skia::FillRule::Winding,
                            transform,
                            clip_mask,
                        );
                    }
                }
            }

            x_cursor += (char_width + gs.char_space) / h_scale;
            if c == ' ' {
                x_cursor += gs.word_space / h_scale;
            }
        }

        Ok(x_cursor * h_scale)
    }
}
