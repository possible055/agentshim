use super::*;

impl TextRasterizer {
    /// Render Unicode text using shaped glyphs.
    /// Returns the total horizontal advance in PDF points.
    pub(super) fn render_unicode_text(
        &self,
        pixmap: &mut Pixmap,
        text: &str,
        bytes: &[u8],
        font_info: Option<&crate::fonts::FontInfo>,
        _font_id: Option<fontdb::ID>,
        font_data: Arc<Vec<u8>>,
        index: u32,
        paint: &Paint,
        base_transform: Transform,
        gs: &GraphicsState,
        clip_mask: Option<&tiny_skia::Mask>,
        pdf_font_name: &str,
        allow_fallback: bool,
    ) -> Result<f32> {
        let font_size = gs.font_size;
        let h_scale = gs.horizontal_scaling / 100.0;

        let font_ref = match harfrust::FontRef::from_index(&font_data, index) {
            Ok(font) => font,
            Err(_) => {
                if allow_fallback {
                    log::warn!("Failed to create harfrust font from embedded data for '{}', falling back to system font", pdf_font_name);
                    if let Some((fallback_id, fallback_data, fallback_index)) =
                        self.load_font_data(pdf_font_name)
                    {
                        return self.render_unicode_text(
                            pixmap,
                            text,
                            bytes,
                            font_info,
                            Some(fallback_id),
                            fallback_data,
                            fallback_index,
                            paint,
                            base_transform,
                            gs,
                            clip_mask,
                            pdf_font_name,
                            false,
                        );
                    }
                }
                return self.render_text_fallback(
                    pixmap,
                    text,
                    paint,
                    base_transform,
                    gs,
                    clip_mask,
                );
            }
        };
        let ttf_face = ttf_parser::Face::parse(&font_data, index)
            .map_err(|_| Error::InvalidPdf(format!("Failed to parse font: {pdf_font_name}")))?;
        let units_per_em = ttf_face.units_per_em() as f32;
        let shaper_data = harfrust::ShaperData::new(&font_ref);

        // 2. Buffer setup
        let mut buffer = harfrust::UnicodeBuffer::new();
        buffer.push_str(text);

        // Explicitly set script and direction for better CJK shaping
        if text
            .chars()
            .any(|c| (c as u32) >= 0x4E00 && (c as u32) <= 0x9FFF)
        {
            if let Some(script) = harfrust::Script::from_iso15924_tag(harfrust::Tag::new(b"Hani")) {
                buffer.set_script(script);
            }
        }
        buffer.set_direction(harfrust::Direction::LeftToRight);
        // Fill in any still-unset segment properties (script for non-CJK,
        // language) so the shaper picks the right GSUB/GPOS rules. The
        // explicit direction and CJK script set above are preserved.
        buffer.guess_segment_properties();

        // 3. Shape the text
        let shaper = shaper_data.shaper(&font_ref).instance(None).build();
        let glyphs = shaper.shape(buffer, harfrust::ShapeOptions::new());
        let info = glyphs.glyph_infos();
        let pos = glyphs.glyph_positions();

        let scale = font_size / units_per_em;
        log::debug!(
            "render_unicode_text: pdf_font={}, units_per_em={}, font_size={}, scale={}",
            pdf_font_name,
            units_per_em,
            font_size,
            scale
        );

        // 4. Transform setup - include full text matrix [Tm]
        let text_transform = Transform::from_row(
            gs.text_matrix.a,
            gs.text_matrix.b,
            gs.text_matrix.c,
            gs.text_matrix.d,
            gs.text_matrix.e,
            gs.text_matrix.f,
        );
        // Transform from text space to pixel space: P_pixel = base_transform * text_transform * P_text
        let combined_base = base_transform.pre_concat(text_transform);

        let mut x_cursor: f32 = 0.0; // In text space units
                                     // y_cursor tracks the cursor along the y-axis. It stays at 0 in
                                     // horizontal mode (the default) and accumulates `w1y*font_size/1000`
                                     // per glyph when WMode 1 is active. Single cursor variable keeps the
                                     // hot loop simple — the branch on `gs.text_wmode` only flips which
                                     // axis receives the advance and how the glyph is positioned
                                     // relative to its horizontal origin.
        let mut y_cursor: f32 = 0.0;
        let mut last_fallback_cluster: Option<usize> = None;
        let wmode = gs.text_wmode;

        // Pre-resolve CIDs for Type0 fonts using our iterator
        let cids: Vec<u16> = if let Some(info) = font_info {
            if info.subtype == "Type0" {
                TextCharIter::new(bytes, Some(info))
                    .map(|(cid, _)| cid)
                    .collect()
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };

        // Build mapping from Unicode byte offset → character index for correct CID lookup.
        // Rustybuzz clusters are byte offsets into the Unicode string, but we need
        // the character index to map to the corresponding CID.
        let cluster_to_char_idx: HashMap<usize, usize> = text
            .char_indices()
            .enumerate()
            .map(|(char_idx, (byte_offset, _))| (byte_offset, char_idx))
            .collect();

        // 5. Iterate through shaped glyphs
        for i in 0..info.len() {
            let glyph_id = info[i].glyph_id;
            let cluster = info[i].cluster as usize;

            // Get character at this cluster (byte offset)
            let char_at_pos = text[cluster..].chars().next().unwrap_or(' ');

            // Map cluster (Unicode byte offset) to character index
            let char_idx = cluster_to_char_idx.get(&cluster).copied().unwrap_or(0);

            // Determine how many *source* characters this glyph represents.
            // For normal 1:1 glyphs, cluster_chars == 1. For shaped
            // ligatures like the "ffi" glyph (#331 R2), one glyph covers
            // multiple characters and harfrust reports them with the
            // same cluster index on every glyph of the cluster. Since we
            // advance the output cursor by the sum of the PDF-declared
            // widths of the *source* characters (per PDF §9.2.4 text-
            // showing advance), we must add the widths of every source
            // character in the ligature cluster to the cursor, not just
            // the first character's width. Otherwise a ligature glyph
            // draws wide but only advances by one character's worth, and
            // subsequent glyphs overwrite the tail of the ligature —
            // exactly the `Efficient` → `Effi ert` symptom reported in
            // #331 on arxiv-style LaTeX-embedded fonts.
            let next_cluster_byte: usize = info
                .get(i + 1)
                .map(|n| n.cluster as usize)
                .unwrap_or(text.len());
            let cluster_chars: usize = text[cluster..next_cluster_byte.min(text.len())]
                .chars()
                .count()
                .max(1);

            // PDF Spec: tx = ((w0 * Tfs) + Tc + Tw) * Th
            // Priority:
            // 1. Explicit /W or /DW from FontInfo (in 1000ths of em),
            //    summed across every source character in the cluster
            //    so ligatures advance by the full cluster's width.
            // 2. Shaped advance from harfrust (fallback, already
            //    reflects the ligature's real width because it comes
            //    from the font's horizontal metrics table).
            let pdf_width = if let Some(font_info_ref) = font_info {
                let mut sum = 0.0_f32;
                for k in 0..cluster_chars {
                    let idx = char_idx + k;
                    let char_code = if font_info_ref.subtype == "Type0" {
                        *cids.get(idx).unwrap_or(&0)
                    } else {
                        *bytes.get(idx).unwrap_or(&0) as u16
                    };
                    sum += font_info_ref.get_glyph_width(char_code);
                }
                sum
            } else {
                // No FontInfo, use shaped advance
                pos[i].x_advance as f32 / font_size * 1000.0
            };

            let x_advance = pdf_width * font_size / 1000.0;
            let x_offset = pos[i].x_offset as f32 / units_per_em * font_size;
            let y_offset = pos[i].y_offset as f32 / units_per_em * font_size;

            let mut x_advance_override: Option<f32> = None;

            // Resolve vertical-mode displacement and origin offset once per
            // glyph. Horizontal mode: y_step = 0, paint_origin_dx/dy = 0 —
            // the same code path as before. Vertical mode: y_step =
            // w1y*Tfs/1000 (typically -font_size), and (paint_origin_dx,
            // paint_origin_dy) shifts the glyph so its vertical origin
            // (v_x, v_y) lands at the current cursor.
            //
            // For composite (Type0) vertical text the per-glyph metrics
            // come from /W2 + /DW2. Simple fonts in vertical mode are not
            // a real-world case but the helper still produces spec-default
            // metrics, keeping the math safe.
            let (y_step, paint_origin_dx, paint_origin_dy) = if wmode == 1 {
                if let Some(font_info_ref) = font_info {
                    // Sum w1y across the source-character cluster, matching the
                    // horizontal path's `pdf_width` accumulation. Use the
                    // primary glyph's vertical-origin offset (v_x, v_y) for
                    // painting — clusters share a single origin per spec.
                    let mut w1y_sum = 0.0_f32;
                    let mut head_v_x = 0.0_f32;
                    let mut head_v_y = 0.0_f32;
                    for k in 0..cluster_chars {
                        let idx = char_idx + k;
                        let cid = if font_info_ref.subtype == "Type0" {
                            *cids.get(idx).unwrap_or(&0)
                        } else {
                            *bytes.get(idx).unwrap_or(&0) as u16
                        };
                        let m = font_info_ref.get_vertical_metrics(cid);
                        w1y_sum += m.w1y;
                        if k == 0 {
                            head_v_x = m.v_x;
                            head_v_y = m.v_y;
                        }
                    }
                    let y_advance_v = w1y_sum * font_size / 1000.0;
                    let dx = -head_v_x * font_size / 1000.0;
                    let dy = -head_v_y * font_size / 1000.0;
                    (y_advance_v, dx, dy)
                } else {
                    // No FontInfo + vertical mode: spec defaults (-1000, 500, 880).
                    let m = crate::fonts::VerticalMetrics::SPEC_DEFAULT;
                    (
                        m.w1y * font_size / 1000.0,
                        -m.v_x * font_size / 1000.0,
                        -m.v_y * font_size / 1000.0,
                    )
                }
            } else {
                (0.0, 0.0, 0.0)
            };

            // Try to get glyph from primary font
            let mut pb = PathBuilder::new();
            let mut builder = SkiaOutlineBuilder(&mut pb);
            let mut has_outline = ttf_face
                .outline_glyph(ttf_parser::GlyphId(glyph_id as u16), &mut builder)
                .is_some();

            if has_outline && glyph_id != 0 {
                if let Some(path) = pb.finish() {
                    // Vertical mode shifts the glyph by (-v_x, -v_y) so its
                    // vertical origin lands at the current cursor, and uses
                    // y_cursor in place of the y=0 baseline. text_rise (Ts)
                    // continues to offset perpendicular to the writing axis
                    // per §9.3.5 — horizontal in vertical mode.
                    let (rise_x, rise_y) = if wmode == 0 {
                        (0.0, gs.text_rise)
                    } else {
                        (gs.text_rise, 0.0)
                    };
                    let px = (x_cursor + x_offset + paint_origin_dx) * h_scale + rise_x;
                    let py = y_cursor + y_offset + paint_origin_dy + rise_y;
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
            } else {
                // FALLBACK PATH: If primary font fails, use the cluster offset to find the original character
                // char_at_pos already retrieved above using byte offset

                // Skip empty glyphs for spaces — advance along the active
                // writing axis (x in horizontal mode, y in vertical mode).
                if char_at_pos.is_whitespace() {
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
                    continue;
                }

                // IMPORTANT: Only render fallback character ONCE per cluster
                if last_fallback_cluster == Some(cluster) {
                    if wmode == 0 {
                        x_cursor += x_advance;
                    } else {
                        y_cursor += y_step;
                    }
                    continue;
                }
                last_fallback_cluster = Some(cluster);

                // Try to find character in fallback CJK fonts.
                // get_cjk_fallback_cached() hits a process-wide OnceLock after the
                // first call — no fontdb queries or font clones on subsequent glyphs.
                if let Some((_, cjk_data, cjk_index)) = get_cjk_fallback_cached(self.font_db()) {
                    if let Ok(cjk_face) = ttf_parser::Face::parse(&cjk_data, cjk_index) {
                        if let Some(cjk_glyph_id) = cjk_face.glyph_index(char_at_pos) {
                            let mut cjk_pb = PathBuilder::new();
                            let mut cjk_builder = SkiaOutlineBuilder(&mut cjk_pb);
                            if cjk_face
                                .outline_glyph(cjk_glyph_id, &mut cjk_builder)
                                .is_some()
                            {
                                if let Some(cjk_path) = cjk_pb.finish() {
                                    let cjk_units_per_em = cjk_face.units_per_em() as f32;
                                    let cjk_scale = font_size / cjk_units_per_em;
                                    let (rise_x, rise_y) = if wmode == 0 {
                                        (0.0, gs.text_rise)
                                    } else {
                                        (gs.text_rise, 0.0)
                                    };
                                    let px =
                                        (x_cursor + x_offset + paint_origin_dx) * h_scale + rise_x;
                                    let py = y_cursor + y_offset + paint_origin_dy + rise_y;
                                    let cjk_transform = combined_base
                                        .pre_translate(px, py)
                                        .pre_scale(cjk_scale, -cjk_scale);
                                    pixmap.fill_path(
                                        &cjk_path,
                                        paint,
                                        tiny_skia::FillRule::Winding,
                                        cjk_transform,
                                        clip_mask,
                                    );
                                    has_outline = true;

                                    if let Some(adv) = cjk_face.glyph_hor_advance(cjk_glyph_id) {
                                        x_advance_override =
                                            Some(adv as f32 / cjk_units_per_em * font_size);
                                    }
                                }
                            }
                        }
                    }
                }

                if !has_outline {
                    log::debug!(
                        "No glyph outline found for char='{}' (0x{:X})",
                        char_at_pos,
                        char_at_pos as u32
                    );
                }
            }

            // Advance cursor in text space per ISO 32000-1:2008 §9.4.4.
            // Horizontal mode: tx = ((w0 * Tfs) + Tc + Tw) * Th
            // Vertical mode:  ty = (w1y * Tfs) + Tc + Tw (Tw applied at the
            // space CID just as in horizontal mode).
            // x_advance / y_step already include w0*Tfs / w1y*Tfs.
            if wmode == 0 {
                x_cursor += x_advance_override.unwrap_or(x_advance);
                x_cursor += gs.char_space;
                if char_at_pos == ' ' {
                    x_cursor += gs.word_space;
                }
            } else {
                y_cursor += y_step;
                y_cursor += gs.char_space;
                if char_at_pos == ' ' {
                    y_cursor += gs.word_space;
                }
            }
        }

        // Return the magnitude of the accumulated advance along the active
        // writing axis. Callers that drive the text matrix forward consume
        // this as a scalar; in vertical mode the cursor advances in y but
        // the magnitude is identically meaningful to the matrix-update
        // helper (which itself handles the axis swap).
        Ok(if wmode == 0 { x_cursor } else { y_cursor })
    }
}
