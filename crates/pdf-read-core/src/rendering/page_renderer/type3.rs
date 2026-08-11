use super::*;

impl PageRenderer {
    /// Returns `true` when the font currently selected in `gs` is a Type 3
    /// font. Type 3 glyphs are user-defined content streams, rendered by
    /// [`Self::render_type3_text`] rather than the outline rasteriser.
    pub(super) fn current_font_is_type3(&self, gs: &GraphicsState) -> bool {
        gs.font_name
            .as_deref()
            .and_then(|n| self.fonts.get(n))
            .map(|f| f.subtype == "Type3")
            .unwrap_or(false)
    }

    /// Render a Type 3 `TJ` array: each string element paints glyphs and each
    /// numeric element shifts the cursor by `-offset/1000 × Tfs` along the
    /// writing axis. Returns the total text-space advance; the caller applies
    /// it once via `advance_text_matrix`.
    pub(super) fn render_type3_tj_array(
        &mut self,
        pixmap: &mut Pixmap,
        array: &[crate::content::operators::TextElement],
        base_transform: Transform,
        gs: &GraphicsState,
        doc: &PdfDocument,
        page_num: usize,
        resources: &Object,
    ) -> Result<f32> {
        use crate::content::operators::TextElement;
        // A local graphics-state copy tracks the cursor across elements; the
        // real text matrix is advanced once by the caller with the returned sum.
        let mut gs_local = gs.clone();
        let mut total = 0.0f32;
        for element in array {
            match element {
                TextElement::String(text) => {
                    let adv = self.render_type3_text(
                        pixmap,
                        text,
                        base_transform,
                        &gs_local,
                        doc,
                        page_num,
                        resources,
                    )?;
                    gs_local.advance_text_matrix(adv);
                    total += adv;
                }
                TextElement::Offset(offset) => {
                    let shift = (-offset / 1000.0) * gs_local.font_size;
                    gs_local.advance_text_matrix(shift);
                    total += shift;
                }
            }
        }
        Ok(total)
    }

    /// Render one Type 3 text string. For each byte code the glyph name is
    /// resolved through the font's `/Encoding` `/Differences`, its `/CharProcs`
    /// content stream is executed under `FontMatrix × text-space × CTM`
    /// (ISO 32000-1 §9.6.5) using the font's own `/Resources`, and the cursor
    /// is advanced by the glyph width. Returns the total text-space advance.
    pub(super) fn render_type3_text(
        &mut self,
        pixmap: &mut Pixmap,
        text: &[u8],
        base_transform: Transform,
        gs: &GraphicsState,
        doc: &PdfDocument,
        page_num: usize,
        resources: &Object,
    ) -> Result<f32> {
        let font_name = match gs.font_name.as_deref() {
            Some(n) => n,
            None => return Ok(0.0),
        };
        let font_info = match self.fonts.get(font_name) {
            Some(f) => Arc::clone(f),
            None => return Ok(0.0),
        };

        // Total advance for the whole string. Computed up front so it is
        // applied even when individual glyph descriptions are missing, and
        // shared with the outline text path for a consistent cursor.
        let string_advance = self.text_rasterizer.measure_text(text, gs, &self.fonts);

        // Resolve the raw Type 3 font dictionary from the resource tree.
        let font_dict_obj = resources
            .as_dict()
            .and_then(|rd| rd.get("Font"))
            .and_then(|f| doc.resolve_object(f).ok())
            .and_then(|fonts| fonts.as_dict().and_then(|fd| fd.get(font_name)).cloned())
            .and_then(|fref| doc.resolve_object(&fref).ok());
        let font_dict = match font_dict_obj.as_ref().and_then(|o| o.as_dict()) {
            Some(d) => d,
            None => return Ok(string_advance),
        };

        // Glyph-space → text-space FontMatrix (default 1/1000 em, Type 1-like).
        let font_matrix = type3_font_matrix(font_dict);

        // /CharProcs (glyph name → content stream).
        let char_procs_obj = font_dict
            .get("CharProcs")
            .and_then(|o| doc.resolve_object(o).ok());
        let char_procs = match char_procs_obj.as_ref().and_then(|o| o.as_dict()) {
            Some(cp) => cp,
            None => return Ok(string_advance),
        };

        // The font's own /Resources, falling back to the page/form resources.
        let font_resources = font_dict
            .get("Resources")
            .and_then(|o| doc.resolve_object(o).ok())
            .unwrap_or_else(|| resources.clone());

        // combined_base = base · CTM · Tm  (user→device · text matrix).
        let transform = combine_transforms(base_transform, &gs.ctm);
        let tm = &gs.text_matrix;
        let combined_base =
            transform.pre_concat(Transform::from_row(tm.a, tm.b, tm.c, tm.d, tm.e, tm.f));

        let font_size = gs.font_size;
        let h_scale = gs.horizontal_scaling / 100.0;
        // Glyphs are suppressed for the invisible / clip-only render modes.
        let paint_glyphs = gs.render_mode != 3 && gs.render_mode != 7;

        // Load the Type 3 font's own resources into the font / colour-space
        // caches for the duration of the glyph descriptions (mirrors the Form
        // XObject path so CharProcs that reference fonts / XObjects resolve).
        let saved_fonts = self.fonts.clone();
        let saved_color_spaces = self.color_spaces.clone();
        let _ = self.load_resources(doc, &font_resources);

        let mut x_cursor = 0.0f32;
        for &code in text {
            let glyph_adv = font_info.get_glyph_width(code as u16) * font_size / 1000.0;

            if paint_glyphs {
                if let Some(name) = font_info.diff_glyph_names.get(&code) {
                    if let Some(stream) = char_procs.get(name) {
                        if let Some(data) = decode_type3_charproc(doc, stream) {
                            // Glyph placement: combined_base · translate(cursor)
                            // · scale(Tfs) · FontMatrix. The cursor is the
                            // un-scaled x position with Th applied at placement,
                            // matching the outline text path.
                            let px = x_cursor * h_scale;
                            let glyph_transform = combined_base
                                .pre_translate(px, gs.text_rise)
                                .pre_scale(font_size, font_size)
                                .pre_concat(font_matrix);
                            let _ = self.render_type3_glyph(
                                pixmap,
                                &data,
                                glyph_transform,
                                doc,
                                page_num,
                                &font_resources,
                                gs.fill_color_rgb,
                            );
                        }
                    }
                }
            }

            x_cursor += glyph_adv + gs.char_space;
            if code == 0x20 {
                x_cursor += gs.word_space;
            }
        }

        self.fonts = saved_fonts;
        self.color_spaces = saved_color_spaces;
        Ok(string_advance)
    }

    /// Execute a single Type 3 glyph description under `glyph_transform`. The
    /// first glyph operator selects the colour model: `d1` marks a stencil
    /// painted with the current fill colour (all colour operators inside are
    /// ignored), while `d0` lets the glyph set its own colours (ISO 32000-1
    /// §9.6.5.2). Malformed streams and over-deep recursion are skipped.
    pub(super) fn render_type3_glyph(
        &mut self,
        pixmap: &mut Pixmap,
        data: &[u8],
        glyph_transform: Transform,
        doc: &PdfDocument,
        page_num: usize,
        resources: &Object,
        fill_rgb: (f32, f32, f32),
    ) -> Result<()> {
        if self.type3_depth >= MAX_TYPE3_DEPTH {
            return Ok(());
        }
        let operators = match parse_content_stream(data) {
            Ok(ops) => ops,
            Err(_) => return Ok(()), // malformed glyph — skip, width already applied
        };

        // Detect the d0 / d1 metric operator (parsed as `Other`). `d1` locks
        // the fill colour; `d0` leaves the glyph free to set its own colours.
        let is_d1 = operators
            .iter()
            .find_map(|op| match op {
                Operator::Other { name, .. } if name == "d1" => Some(true),
                Operator::Other { name, .. } if name == "d0" => Some(false),
                _ => None,
            })
            .unwrap_or(false);

        self.type3_depth += 1;
        let prev_lock = self.type3_fill_lock.take();
        if is_d1 {
            self.type3_fill_lock = Some(fill_rgb);
        }
        let result = self.execute_operators(
            pixmap,
            glyph_transform,
            &operators,
            doc,
            page_num,
            resources,
        );
        self.type3_fill_lock = prev_lock;
        self.type3_depth -= 1;
        result
    }
}
