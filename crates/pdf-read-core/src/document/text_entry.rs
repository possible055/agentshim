use super::parsing::*;
use super::preflight::*;
use super::*;

impl PdfDocument {
    /// Recursively find page reference in the page tree.
    pub(crate) fn get_page_ref_recursive(
        &self,
        node_ref: ObjectRef,
        target_index: usize,
        current_index: &mut usize,
        visited: &mut HashSet<ObjectRef>,
    ) -> Result<ObjectRef> {
        if !visited.insert(node_ref) {
            return Err(Error::CircularReference(node_ref));
        }
        let node = self.load_object(node_ref)?;
        let node_dict = match node.as_dict() {
            Some(d) => d,
            None => {
                log::warn!(
                    "Page tree node {} is {} (expected Dictionary), skipping",
                    node_ref.id,
                    node.type_name()
                );
                return Err(Error::InvalidPdf(format!(
                    "Page tree node {} is not a dictionary",
                    node_ref.id
                )));
            }
        };

        let node_type = node_dict
            .get("Type")
            .and_then(|t| t.as_name())
            .ok_or_else(|| Error::InvalidPdf("Node missing Type".to_string()))?;

        match node_type {
            "Page" => {
                if *current_index == target_index {
                    Ok(node_ref)
                } else {
                    *current_index += 1;
                    Err(Error::InvalidPdf(format!(
                        "Page {} not found",
                        target_index
                    )))
                }
            }
            "Pages" => {
                let kids = node_dict
                    .get("Kids")
                    .and_then(|k| k.as_array())
                    .ok_or_else(|| Error::InvalidPdf("Pages node missing Kids".to_string()))?;

                for kid_obj in kids {
                    if let Some(kid_ref) = kid_obj.as_reference() {
                        match self.get_page_ref_recursive(
                            kid_ref,
                            target_index,
                            current_index,
                            visited,
                        ) {
                            Ok(page_ref) => return Ok(page_ref),
                            Err(_) => continue,
                        }
                    }
                }

                Err(Error::InvalidPdf(format!(
                    "Page {} not found",
                    target_index
                )))
            }
            _ => Err(Error::InvalidPdf(format!(
                "Unknown node type: {}",
                node_type
            ))),
        }
    }

    /// Extract text from a page as a plain string.
    ///
    /// # Arguments
    ///
    /// * `page_index` - Zero-based page index
    ///
    /// # Returns
    ///
    /// The extracted text as a string.
    ///
    /// # Errors
    ///
    /// Returns an error if the page cannot be accessed or text extraction fails.
    /// Decode PDF escape sequences in text (e.g., \274 -> §, \( -> (, etc.)
    #[allow(dead_code)]
    pub(super) fn decode_pdf_escapes(text: &str) -> String {
        let mut result = String::with_capacity(text.len());
        let mut chars = text.chars().peekable();

        while let Some(ch) = chars.next() {
            if ch == '\\' {
                // Check what follows the backslash
                match chars.peek() {
                    Some(&'(') => {
                        result.push('(');
                        chars.next();
                    }
                    Some(&')') => {
                        result.push(')');
                        chars.next();
                    }
                    Some(&'\\') => {
                        result.push('\\');
                        chars.next();
                    }
                    Some(&'n') => {
                        result.push('\n');
                        chars.next();
                    }
                    Some(&'r') => {
                        result.push('\r');
                        chars.next();
                    }
                    Some(&'t') => {
                        result.push('\t');
                        chars.next();
                    }
                    Some(&'?') => {
                        // \? is a soft hyphen (optional line break point)
                        // Just skip it
                        chars.next();
                    }
                    Some(d) if d.is_ascii_digit() => {
                        // Octal escape sequence: \ddd
                        let mut octal = String::new();
                        for _ in 0..3 {
                            if let Some(&digit) = chars.peek() {
                                if digit.is_ascii_digit() && digit < '8' {
                                    octal.push(digit);
                                    chars.next();
                                } else {
                                    break;
                                }
                            } else {
                                break;
                            }
                        }

                        if !octal.is_empty() {
                            if let Ok(code) = u8::from_str_radix(&octal, 8) {
                                // PDFDocEncoding: ISO 32000-1:2008, Annex D
                                let decoded_char = Self::pdfdoc_decode(code);
                                result.push(decoded_char);
                            } else {
                                // Failed to parse, keep the backslash and octal
                                result.push('\\');
                                result.push_str(&octal);
                            }
                        } else {
                            // No valid octal digits, keep the backslash
                            result.push('\\');
                        }
                    }
                    _ => {
                        // Unknown escape, keep the backslash
                        result.push('\\');
                    }
                }
            } else {
                result.push(ch);
            }
        }

        result
    }

    /// Decode a byte using PDFDocEncoding (ISO 32000-1:2008, Annex D).
    ///
    /// PDFDocEncoding is the default encoding for text strings in PDF:
    /// - Codes 0-127: ASCII
    /// - Codes 128-159: Special Unicode characters
    /// - Codes 160-255: Latin-1 (ISO 8859-1)
    #[allow(dead_code)]
    pub(super) fn pdfdoc_decode(code: u8) -> char {
        match code {
            // 0-127: Standard ASCII
            0..=127 => code as char,

            // 128-159: PDFDocEncoding special mappings
            128 => '\u{2022}', // BULLET
            129 => '\u{2020}', // DAGGER
            130 => '\u{2021}', // DOUBLE DAGGER
            131 => '\u{2026}', // HORIZONTAL ELLIPSIS
            132 => '\u{2014}', // EM DASH
            133 => '\u{2013}', // EN DASH
            134 => '\u{0192}', // LATIN SMALL LETTER F WITH HOOK
            135 => '\u{2044}', // FRACTION SLASH
            136 => '\u{2039}', // SINGLE LEFT-POINTING ANGLE QUOTATION MARK
            137 => '\u{203A}', // SINGLE RIGHT-POINTING ANGLE QUOTATION MARK
            138 => '\u{2212}', // MINUS SIGN
            139 => '\u{2030}', // PER MILLE SIGN
            140 => '\u{201E}', // DOUBLE LOW-9 QUOTATION MARK
            141 => '\u{201C}', // LEFT DOUBLE QUOTATION MARK
            142 => '\u{201D}', // RIGHT DOUBLE QUOTATION MARK
            143 => '\u{2018}', // LEFT SINGLE QUOTATION MARK
            144 => '\u{2019}', // RIGHT SINGLE QUOTATION MARK
            145 => '\u{201A}', // SINGLE LOW-9 QUOTATION MARK
            146 => '\u{2122}', // TRADE MARK SIGN
            147 => '\u{FB01}', // LATIN SMALL LIGATURE FI
            148 => '\u{FB02}', // LATIN SMALL LIGATURE FL
            149 => '\u{0141}', // LATIN CAPITAL LETTER L WITH STROKE
            150 => '\u{0152}', // LATIN CAPITAL LIGATURE OE
            151 => '\u{0160}', // LATIN CAPITAL LETTER S WITH CARON
            152 => '\u{0178}', // LATIN CAPITAL LETTER Y WITH DIAERESIS
            153 => '\u{017D}', // LATIN CAPITAL LETTER Z WITH CARON
            154 => '\u{0131}', // LATIN SMALL LETTER DOTLESS I
            155 => '\u{0142}', // LATIN SMALL LETTER L WITH STROKE
            156 => '\u{0153}', // LATIN SMALL LIGATURE OE
            157 => '\u{0161}', // LATIN SMALL LETTER S WITH CARON
            158 => '\u{017E}', // LATIN SMALL LETTER Z WITH CARON
            159 => '\u{FFFD}', // REPLACEMENT CHARACTER (undefined in PDFDocEncoding)

            // 160-255: Latin-1 (ISO 8859-1)
            160..=255 => code as char,
        }
    }

    /// Circular references and recursion limit errors are handled gracefully
    /// with warning messages in the output.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use pdf_oxide::document::PdfDocument;
    /// # let mut doc = PdfDocument::open("sample.pdf")?;
    /// let text = doc.extract_text(0)?;
    /// println!("Page 1 text: {}", text);
    /// # Ok::<(), pdf_oxide::error::Error>(())
    /// ```
    ///
    /// # Extract text from a page
    ///
    /// pdf_oxide exposes three plain-text surfaces with different strengths
    /// (#554). Pick by document shape:
    ///
    /// - `extract_text(page)` (this method) — glyph-walk assembly with
    ///   row-aware ordering, inline table rendering, and artifact filtering.
    ///   The most discoverable default; strongest on single-column prose.
    /// - `to_plain_text(page, opts)` / `to_plain_text_all(opts)` — runs the
    ///   full pipeline (reading-order strategy incl. XY-cut). Best on
    ///   multi-column / complex layouts where reading order matters.
    /// - `to_markdown_all(opts)` then strip markup — preserves structure
    ///   (headings, lists, tables) and often scores highest on heavily
    ///   structured documents; lossiest for pure prose.
    ///
    /// No single mode wins on every PDF; when extraction quality is critical
    /// and the layout is unknown, compare `to_plain_text_all` and
    /// markdown-stripped output and keep whichever is better for your corpus.
    pub fn extract_text(&self, page_index: usize) -> Result<String> {
        // Enable table extraction so that tabular content is preserved as
        // space-padded, column-aligned rows (see Table::render_text).
        let options = crate::converters::ConversionOptions {
            extract_tables: true,
            ..Default::default()
        };
        self.extract_text_with_options(page_index, &options)
    }

    /// Extract text from a page with specific options (v0.3.16).
    pub fn extract_text_with_options(
        &self,
        page_index: usize,
        options: &crate::converters::ConversionOptions,
    ) -> Result<String> {
        let base_spans = self.extract_spans(page_index)?;
        // Vertical CJK (tategaki, ISO 32000-1 §9.7.4.3 vertical writing mode):
        // glyphs run top-to-bottom in columns that progress right-to-left, so
        // the horizontal row-major assembler shreds the reading order. When the
        // page is geometrically vertical, read it column-major instead.
        if let Some(vertical) = Self::try_assemble_vertical_cjk(&base_spans) {
            return Ok(vertical);
        }
        // Dominant text-matrix rotation (a landscape table typeset on a
        // portrait page): the row-major assembler groups lines
        // in the portrait frame and interleaves every rotated row. Assemble
        // such pages in their rotated reading frame instead.
        let base_spans = match self.map_dominant_rotation_into_reading_frame(page_index, base_spans)
        {
            Ok(mapped) => mapped,
            Err(original) => original,
        };
        let text = self.assemble_text_from_spans(page_index, base_spans, options)?;
        Ok(Self::apply_mixed_rtl_line_pass(text))
    }

    /// Map a dominant-rotation page's spans into their rotated reading
    /// frame so the standard horizontal assembler applies.
    ///
    /// Returns `Ok(mapped)` when the page is unrotated (`/Rotate 0` — on
    /// rotated pages `postprocess_spans` already handles content rotation,
    ///) and at least half its non-whitespace spans share one
    /// quadrant text rotation; the mapped spans are horizontal in the
    /// frame a reader turns the page into, with `rotation_degrees` cleared
    /// so downstream passes treat them as the upright text they now are.
    /// Returns `Err(spans)` — the input unchanged — on every other page,
    /// keeping output byte-identical there.
    ///
    /// Only used for plain-text assembly, where no coordinates leak to the
    /// caller; coordinate-bearing APIs (`extract_words`) reorder in the
    /// rotated frame but report true page-space bboxes instead (see
    /// `crate::pipeline::page_reading_order`).
    pub(super) fn map_dominant_rotation_into_reading_frame(
        &self,
        page_index: usize,
        spans: Vec<crate::layout::TextSpan>,
    ) -> std::result::Result<Vec<crate::layout::TextSpan>, Vec<crate::layout::TextSpan>> {
        if self.get_page_rotation(page_index).unwrap_or(0) != 0 {
            return Err(spans);
        }
        let Some(deg) = crate::utils::dominant_rotation(&spans) else {
            return Err(spans);
        };
        // Same quadrant mapping as the word path: 90° text reads upright
        // under a /Rotate-90-style display transform, -90° under 270,
        // 180° under 180. Mirrored / free-angle runs have no frame.
        let rot = if (deg - 90.0).abs() < 0.5 {
            90
        } else if (deg - 180.0).abs() < 0.5 {
            180
        } else if (deg + 90.0).abs() < 0.5 {
            270
        } else {
            return Err(spans);
        };
        log::debug!(
            "page {page_index}: dominant text rotation {deg}° — assembling text in rotated frame"
        );
        let (llx, lly, urx, ury) = self
            .get_page_media_box(page_index)
            .unwrap_or((0.0, 0.0, 612.0, 792.0));
        let (w, h) = (urx - llx, ury - lly);
        let mut spans = spans;
        // Rotated spans store TEXT-LOCAL extents (origin + advance-along-
        // the-run as `width` + font size as `height`): rotate the ORIGIN
        // as a point and keep the extents, which already describe the run
        // in its own upright frame (same convention as
        // `order_rotated_blocks`).
        for s in &mut spans {
            let (rx, ry) = (s.bbox.x - llx, s.bbox.y - lly);
            let (mx, my) = match rot {
                90 => (ry, w - rx),
                180 => (w - rx, h - ry),
                270 => (h - ry, rx),
                _ => (rx, ry),
            };
            s.bbox.x = llx + mx;
            s.bbox.y = lly + my;
            s.rotation_degrees = 0.0;
        }
        Ok(spans)
    }

    /// Returns column-major text when the page is a vertical-CJK (tategaki)
    /// layout, or `None` for every other page (so horizontal documents are
    /// byte-for-byte unchanged).
    ///
    /// Detection is purely geometric: among CJK glyph spans, count how many
    /// neighbour pairs are stacked *vertically* (same column, one glyph-height
    /// apart) versus *horizontally* (same row, one glyph-width apart). Vertical
    /// writing is declared only when CJK is the clear majority of the page and
    /// vertical adjacencies dominate horizontal ones — so horizontal CJK
    /// (Chinese/Japanese prose set left-to-right) never triggers it. Assembly
    /// then orders spans by column right-to-left (X descending, banded to the
    /// glyph width) and top-to-bottom within a column (Y descending), matching
    /// how the script is read.
    pub(super) fn try_assemble_vertical_cjk(spans: &[TextSpan]) -> Option<String> {
        fn is_cjk(c: char) -> bool {
            matches!(
                c as u32,
                0x3040..=0x30FF      // Hiragana + Katakana
                | 0x3400..=0x4DBF    // CJK Ext A
                | 0x4E00..=0x9FFF    // CJK Unified
                | 0xF900..=0xFAFF    // CJK Compatibility
                | 0xFF66..=0xFF9F    // Halfwidth Katakana
            )
        }
        let cjk: Vec<&TextSpan> = spans
            .iter()
            .filter(|s| s.text.chars().any(is_cjk))
            .collect();
        if cjk.len() < 8 {
            return None;
        }
        // Tategaki signature: vertical writing positions each glyph on its own
        // origin, so a genuine vertical-CJK page is composed of SINGLE-glyph
        // spans. Horizontal CJK is emitted as multi-character runs (a whole
        // word/line per show op). The column-major geometry below assumes glyph
        // cells; on run-level spans the nearest neighbour of a run is the run on
        // the line above/below (vertical), so a horizontal page is mis-detected
        // as vertical and its reading order is shredded. Require single-glyph
        // CJK spans to be the majority before treating the page as vertical;
        // otherwise fall back to the horizontal assembler (the pre-vertical-CJK
        // behaviour, so a missed detection never regresses against it).
        let single_glyph_cjk = cjk
            .iter()
            .filter(|s| {
                let t = s.text.trim();
                t.chars().count() == 1 && t.chars().all(is_cjk)
            })
            .count();
        if single_glyph_cjk * 2 < cjk.len() {
            return None;
        }
        // CJK must be the clear majority of the page's non-space glyphs.
        let total_chars: usize = spans
            .iter()
            .map(|s| s.text.chars().filter(|c| !c.is_whitespace()).count())
            .sum();
        let cjk_chars: usize = cjk
            .iter()
            .map(|s| s.text.chars().filter(|c| is_cjk(*c)).count())
            .sum();
        if total_chars == 0 || cjk_chars * 2 < total_chars {
            return None;
        }

        // Glyph cell width from the median-ish span box (CJK glyphs are square);
        // used to band columns when sorting column-major below.
        let mut widths: Vec<f32> = cjk
            .iter()
            .map(|s| s.bbox.width)
            .filter(|w| *w > 0.0)
            .collect();
        if widths.is_empty() {
            return None;
        }
        widths.sort_by(|a, b| crate::utils::safe_float_cmp(*a, *b));
        let gw = widths[widths.len() / 2];

        // Discriminate vertical (tategaki) from horizontal CJK by each glyph's
        // NEAREST-neighbour direction (capped for O(n²) cost). The earlier
        // all-pairs `vert > horiz` count mis-classified grid-aligned HORIZONTAL
        // CJK — genkō-yōshi regulatory/academic text aligns glyphs in both rows
        // and columns, so vertical and horizontal adjacencies are about equal
        // and noise tipped the page to "vertical", scrambling its reading order
        // and collapsing line breaks. A glyph's single closest neighbour is the
        // next glyph in its own line: horizontally adjacent for horizontal text
        // (intra-row pitch < inter-row leading), vertically adjacent for
        // tategaki (intra-column pitch < inter-column gutter). Only assemble
        // column-major when vertical nearest-neighbours clearly dominate
        // (> 2×). Otherwise fall back to the normal horizontal path — which is
        // also the pre-vertical-CJK (≤ 0.3.61) behaviour, so a missed detection
        // never regresses against that baseline.
        let sample = &cjk[..cjk.len().min(250)];
        let (mut vert, mut horiz) = (0usize, 0usize);
        for (i, a) in sample.iter().enumerate() {
            let (mut best, mut bdx, mut bdy) = (f32::MAX, 0.0f32, 0.0f32);
            for (j, b) in sample.iter().enumerate() {
                if i == j {
                    continue;
                }
                let dx = a.bbox.x - b.bbox.x;
                let dy = a.bbox.y - b.bbox.y;
                let d2 = dx * dx + dy * dy;
                if d2 < best {
                    best = d2;
                    bdx = dx.abs();
                    bdy = dy.abs();
                }
            }
            // Classify the nearest neighbour's dominant axis (ties → neither).
            if bdy > bdx {
                vert += 1;
            } else if bdx > bdy {
                horiz += 1;
            }
        }
        // Require a clear vertical majority; ambiguous or horizontal → None.
        if vert == 0 || vert <= horiz * 2 {
            return None;
        }

        // Column-major order: X descending (right-to-left), banded to the glyph
        // width so a column's sub-pixel X jitter does not split it, then Y
        // descending (top-to-bottom) within the column.
        let band = (gw * 0.5).max(1.0);
        let mut ordered: Vec<&TextSpan> = spans.iter().collect();
        ordered.sort_by(|a, b| {
            let ca = (a.bbox.x / band).round() as i32;
            let cb = (b.bbox.x / band).round() as i32;
            cb.cmp(&ca)
                .then(crate::utils::safe_float_cmp(b.bbox.y, a.bbox.y))
        });
        Some(ordered.iter().map(|s| s.text.as_str()).collect())
    }

    /// Per-line UAX #9 pass for mixed-direction lines (bidi item 4): for each
    /// output line that is confidently RTL and mixes Arabic/Hebrew with
    /// European/Arabic-Indic numerals or Latin words (e.g. a date
    /// `14 april 1434 ٤٣٤١`), give the embedded LTR sub-runs their left-to-right
    /// sublevel (UAX #9 §3.3.4) while leaving the already-logical RTL runs fixed.
    /// Gated inside `reorder_mixed_rtl_line`, so pure-RTL, pure-LTR, and
    /// non-RTL lines are returned byte-for-byte unchanged; the ASCII fast path
    /// keeps all Latin-only extraction identical.
    pub(super) fn apply_mixed_rtl_line_pass(text: String) -> String {
        if text.is_ascii() {
            return text;
        }
        text.split('\n')
            .map(crate::text::bidi::reorder_mixed_rtl_line)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Apply caller-specified region filters to a span set: drop spans matching
    /// any `exclude_regions` (under `exclude_regions_mode`), then keep only spans
    /// inside `include_region` if one is set. Exclusion runs first so it takes
    /// precedence. Shared by the plain-text, markdown, and HTML conversion paths
    /// so `ConversionOptions` region filtering behaves identically across every
    /// text surface. A no-op when neither field is set.
    pub(super) fn apply_region_filters(
        base_spans: Vec<crate::layout::TextSpan>,
        options: &crate::converters::ConversionOptions,
    ) -> Vec<crate::layout::TextSpan> {
        use crate::layout::SpatialCollectionFiltering;
        let mut spans = base_spans;
        if !options.exclude_regions.is_empty() {
            spans = spans.exclude_rects(&options.exclude_regions, options.exclude_regions_mode);
        }
        if let Some((ref region, mode)) = options.include_region {
            spans = spans.filter_by_rect(region, mode);
        }
        spans
    }
}
