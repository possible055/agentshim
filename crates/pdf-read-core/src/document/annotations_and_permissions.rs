use super::parsing::*;
use super::preflight::*;
use super::*;

impl PdfDocument {
    /// Append text from non-widget annotations on a page.
    ///
    /// Extracts text from FreeText annotations (text box contents), Stamp annotations
    /// (appearance stream text), and other non-widget annotation types.
    /// Widget annotations are handled separately via `extract_widget_spans()`.
    /// Skips hidden and invisible annotations per PDF spec flags.
    pub(super) fn append_non_widget_annotation_text(&self, page_index: usize, text: &mut String) {
        // Lightweight annotation text extraction — avoids full get_annotations() overhead.
        // Only reads /Subtype, /V, /Contents, /F, and /Parent (for field value inheritance).
        // Uses get_page() which is cached after first access.
        let page_obj = match self.get_page(page_index) {
            Ok(o) => o,
            Err(_) => return,
        };
        let page_dict = match page_obj.as_dict() {
            Some(d) => d,
            None => return,
        };

        // Get /Annots array (may be direct or indirect)
        let annots_arr = match page_dict.get("Annots") {
            Some(Object::Array(arr)) => arr.clone(),
            Some(Object::Reference(r)) => match self.load_object(*r) {
                Ok(Object::Array(arr)) => arr,
                _ => return,
            },
            _ => return, // No annotations on this page
        };

        let mut annot_texts: Vec<String> = Vec::new();

        for annot_obj in &annots_arr {
            let len_before_annot = annot_texts.len();
            let annot_ref = match annot_obj {
                Object::Reference(r) => *r,
                _ => continue,
            };
            let dict = match self.load_object(annot_ref) {
                Ok(obj) => match obj.as_dict() {
                    Some(d) => d.clone(),
                    None => continue,
                },
                Err(_) => continue,
            };

            // Check /F flags — skip invisible/hidden annotations
            // Bit 1 (0x1) = Invisible, Bit 2 (0x2) = Hidden, Bit 6 (0x20) = NoView
            if let Some(Object::Integer(f)) = dict.get("F") {
                if *f & (0x1 | 0x2 | 0x20) != 0 {
                    continue;
                }
            }

            let subtype = match dict.get("Subtype").and_then(|s| s.as_name()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let subtype_lower = subtype.to_ascii_lowercase();

            match subtype_lower.as_str() {
                "widget" => {
                    // Widgets are now handled by extract_widget_spans() as inline TextSpans.
                    // Skip them here to avoid duplicate text at the end of output.
                    continue;
                }
                "freetext" | "stamp" => {
                    if let Some(Object::String(s)) = dict.get("Contents") {
                        let decoded = Self::decode_pdf_text_string(s);
                        let trimmed = decoded.trim().to_string();
                        if !trimmed.is_empty() {
                            annot_texts.push(trimmed);
                        }
                    }
                }
                // Text (sticky-note) /Contents is reviewer popup comment text, not
                // visible page content — skip to avoid injecting popup notes.
                "text" => {}
                // Geometric shape annotations — per §12.5.6.2, their /Contents is
                // also popup/comment text, same as the markup group below.
                "line" | "circle" | "square" | "polygon" | "polyline" => {
                    // Skip — /Contents is popup comment text, not page content.
                }
                // Markup/comment annotations — per ISO 32000-1 §12.5.6.2 (Table 166),
                // the /Contents of all these subtypes is popup/comment text written
                // by a reviewer, NOT text displayed on the page. Exclude to avoid
                // injecting user annotation notes into the body text stream.
                // Per §12.5.6.2, all of these annotations' /Contents is popup/comment
                // text (displayed in a pop-up window), not rendered page content.
                // FileAttachment is explicitly in this category per §12.5.6.2 even
                // though §12.5.6.15 calls it "descriptive text" — the pop-up semantics
                // take precedence.
                "highlight" | "underline" | "strikeout" | "squiggly" | "caret"
                | "fileattachment" | "redact" | "ink" => {
                    // Skip — /Contents is popup comment text, not page content.
                }
                // Link /Contents is an accessibility alternate description (§12.5.6.5).
                // Treated as supplementary text on pages with no body content.
                "link" => {
                    if let Some(Object::String(s)) = dict.get("Contents") {
                        let decoded = Self::decode_pdf_text_string(s);
                        let trimmed = decoded.trim().to_string();
                        if !trimmed.is_empty() {
                            annot_texts.push(trimmed);
                        }
                    }
                }
                // Popup annotations — per §12.5.6.14 Table 183, the parent
                // annotation's /Contents overrides the popup's own /Contents.
                "popup" => {
                    // Try parent annotation's /Contents first (spec §12.5.6.14).
                    let mut got_text = false;
                    if let Some(parent_ref) = dict.get("Parent").and_then(|o| o.as_reference()) {
                        if let Ok(parent_obj) = self.load_object(parent_ref) {
                            if let Some(parent_dict) = parent_obj.as_dict() {
                                if let Some(Object::String(s)) = parent_dict.get("Contents") {
                                    let decoded = Self::decode_pdf_text_string(s);
                                    let trimmed = decoded.trim().to_string();
                                    if !trimmed.is_empty() {
                                        annot_texts.push(trimmed);
                                        got_text = true;
                                    }
                                }
                            }
                        }
                    }
                    // Fall back to the popup's own /Contents only when parent has none.
                    if !got_text {
                        if let Some(Object::String(s)) = dict.get("Contents") {
                            let decoded = Self::decode_pdf_text_string(s);
                            let trimmed = decoded.trim().to_string();
                            if !trimmed.is_empty() {
                                annot_texts.push(trimmed);
                            }
                        }
                    }
                }
                _ => {
                    // For any other annotation type, also try /Contents
                    if let Some(Object::String(s)) = dict.get("Contents") {
                        let decoded = Self::decode_pdf_text_string(s);
                        let trimmed = decoded.trim().to_string();
                        if !trimmed.is_empty() {
                            annot_texts.push(trimmed);
                        }
                    }
                }
            }

            // Fallback: if no text was extracted from /V or /Contents,
            // try extracting from the /AP/N (Normal Appearance) stream.
            let text_before = annot_texts.len();
            if text_before == len_before_annot {
                if let Some(ap_text) = self.extract_text_from_ap_stream(&dict) {
                    let trimmed = ap_text.trim().to_string();
                    if !trimmed.is_empty() {
                        annot_texts.push(trimmed);
                    }
                }
            }
        }

        if !annot_texts.is_empty() {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&annot_texts.join("\n"));
        }
    }

    /// Extract text from an annotation's Normal Appearance stream (/AP/N).
    ///
    /// AP streams are content streams with their own /Resources. This creates
    /// a temporary TextExtractor, loads fonts from the AP stream resources,
    /// and extracts text spans from the decoded stream data.
    pub(super) fn extract_text_from_ap_stream(
        &self,
        annot_dict: &std::collections::HashMap<String, Object>,
    ) -> Option<String> {
        use crate::extractors::TextExtractor;

        // Get /AP dictionary
        let ap_obj = annot_dict.get("AP")?;
        let ap = if let Some(r) = ap_obj.as_reference() {
            self.load_object(r).ok()?
        } else {
            ap_obj.clone()
        };
        let ap_dict = ap.as_dict()?;

        // Get /N (Normal appearance) — can be a stream ref or a dictionary of states
        let n_obj = ap_dict.get("N")?;
        let (n_stream, n_ref) = match n_obj {
            Object::Reference(r) => (self.load_object(*r).ok()?, *r),
            _ => return None, // N must be a reference to a stream
        };

        // Verify it's a stream (has a dict with stream data)
        let n_dict = n_stream.as_dict()?;

        // Decode the AP/N stream
        let stream_data = match self.decode_stream_with_encryption(&n_stream, n_ref) {
            Ok(data) => data,
            Err(_) => return None,
        };

        // Quick check: does the stream contain text operators?
        if !Self::may_contain_text(&stream_data) {
            return None;
        }

        // Create a temporary text extractor for this AP stream
        let mut extractor = TextExtractor::new();

        // Load fonts from the AP/N stream's own /Resources. No resources on
        // the AP stream — try the annotation's /DR or parent page resources
        // — means we can't decode fonts, so bail.
        {
            let resources = n_dict.get("Resources")?;
            let res_obj = if let Some(r) = resources.as_reference() {
                self.load_object(r)
                    .ok()
                    .unwrap_or_else(|| resources.clone())
            } else {
                resources.clone()
            };
            extractor.set_resources(res_obj.clone());
            extractor.set_document(self);
            let _ = self.load_fonts(&res_obj, &mut extractor);
        }

        // Extract text spans from the AP stream
        let spans = extractor.extract_text_spans(&stream_data).ok()?;
        if spans.is_empty() {
            return None;
        }

        // Collect span text
        let text: String = spans
            .iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        if text.trim().is_empty() {
            return None;
        }
        Some(text)
    }

    /// Char-count capacity for what physically fits inside a widget
    /// bbox at body font sizes. Per PDF spec §12.7.4.3 the field's
    /// value is `/V`; the appearance stream is visual rendering
    /// only. When we fall back to AP extraction the result must be
    /// bounded by what the widget could visually show — PDFs that
    /// reuse a single Form XObject for many widgets' `/AP /N` would
    /// otherwise dump the shared content once per widget, and
    /// scrollable multi-line text fields hold far more characters
    /// in `/V` than ever render at once.
    ///
    /// Heuristic: ~14 chars per cm² at body font sizes. At PDF
    /// 72 dpi (1 pt = 0.0353 cm), the formula
    /// `capacity = 0.0175 * w_pt * h_pt + 64` applies; the constant
    /// term absorbs short labels where the area estimate alone is
    /// too tight to even hold the field's name.
    pub(super) fn widget_text_capacity(bbox: &crate::geometry::Rect) -> usize {
        let area = bbox.width.max(0.0) * bbox.height.max(0.0);
        (0.0175 * area) as usize + 64
    }

    /// Truncate `text` to the widget's visual capacity. If `text`
    /// already fits, returns it unchanged. Used to bound AP-fallback
    /// extraction (and other content paths) so a single widget can't
    /// dump page-background prose or scrollable field internals into
    /// the page text.
    pub(super) fn truncate_to_widget_capacity(
        text: String,
        bbox: &crate::geometry::Rect,
    ) -> String {
        let cap = Self::widget_text_capacity(bbox);
        let n = text.chars().count();
        if n <= cap {
            return text;
        }
        text.chars().take(cap).collect()
    }

    /// Walk /Parent chain to find inherited /FT (field type) value.
    pub(super) fn resolve_inherited_ft(
        &self,
        dict: &std::collections::HashMap<String, Object>,
    ) -> Option<String> {
        let mut parent_ref = match dict.get("Parent") {
            Some(Object::Reference(r)) => Some(*r),
            _ => return None,
        };
        let mut depth = 0;
        while let Some(pref) = parent_ref {
            if depth >= 10 {
                break;
            }
            depth += 1;
            if let Ok(parent_obj) = self.load_object(pref) {
                if let Some(parent_dict) = parent_obj.as_dict() {
                    if let Some(ft) = parent_dict.get("FT").and_then(|o| o.as_name()) {
                        return Some(ft.to_string());
                    }
                    parent_ref = match parent_dict.get("Parent") {
                        Some(Object::Reference(r)) => Some(*r),
                        _ => None,
                    };
                } else {
                    break;
                }
            } else {
                break;
            }
        }
        None
    }

    /// Walk /Parent chain to find inherited /V value (PDF spec 12.7.3.1).
    pub(super) fn resolve_inherited_field_value(
        &self,
        dict: &std::collections::HashMap<String, Object>,
    ) -> Option<String> {
        let mut parent_ref = match dict.get("Parent") {
            Some(Object::Reference(r)) => Some(*r),
            _ => return None,
        };
        let mut depth = 0;
        while let Some(pref) = parent_ref {
            if depth >= 10 {
                break;
            }
            depth += 1;
            if let Ok(parent_obj) = self.load_object(pref) {
                if let Some(parent_dict) = parent_obj.as_dict() {
                    if let Some(v) = Self::parse_string_value_static(parent_dict.get("V")) {
                        return Some(v);
                    }
                    parent_ref = match parent_dict.get("Parent") {
                        Some(Object::Reference(r)) => Some(*r),
                        _ => None,
                    };
                } else {
                    break;
                }
            } else {
                break;
            }
        }
        None
    }

    /// Parse a string value from a PDF object with proper PDF string decoding.
    /// Handles UTF-16BE (BOM \xFE\xFF) and PDFDocEncoding per ISO 32000-1 §7.9.2.2.
    pub(super) fn parse_string_value_static(obj: Option<&Object>) -> Option<String> {
        match obj {
            Some(Object::String(s)) => Some(Self::decode_pdf_text_string(s)),
            Some(Object::Name(n)) => Some(n.clone()),
            Some(Object::Integer(i)) => Some(i.to_string()),
            Some(Object::Real(f)) => Some(f.to_string()),
            _ => None,
        }
    }

    /// Decode a PDF text string that may be UTF-16BE/LE (with BOM) or PDFDocEncoding.
    pub(super) fn decode_pdf_text_string(bytes: &[u8]) -> String {
        if bytes.len() >= 2 && bytes[0] == 0xFE && bytes[1] == 0xFF {
            // UTF-16BE with BOM
            let utf16_bytes = &bytes[2..];
            let utf16_pairs: Vec<u16> = utf16_bytes
                .chunks_exact(2)
                .map(|chunk| u16::from_be_bytes([chunk[0], chunk[1]]))
                .collect();
            String::from_utf16(&utf16_pairs)
                .unwrap_or_else(|_| String::from_utf8_lossy(bytes).to_string())
        } else if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xFE {
            // UTF-16LE with BOM
            let utf16_bytes = &bytes[2..];
            let utf16_pairs: Vec<u16> = utf16_bytes
                .chunks_exact(2)
                .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
                .collect();
            String::from_utf16(&utf16_pairs)
                .unwrap_or_else(|_| String::from_utf8_lossy(bytes).to_string())
        } else {
            // PDFDocEncoding — superset of ISO Latin-1
            bytes
                .iter()
                .filter_map(|&b| crate::fonts::font_dict::pdfdoc_encoding_lookup(b))
                .collect()
        }
    }

    /// Check if decoded content stream data may contain text.
    ///
    /// Returns true if the stream contains either:
    /// - A BT (Begin Text) operator (text is directly in the page stream)
    /// - A Do operator (Form XObject invocation that may contain text)
    ///
    /// Per §9.4.3, text-showing operators shall only appear within BT...ET text
    /// objects. However, a page may contain text only inside Form XObjects
    /// referenced via `Do` operators, so we must also check for those.
    pub(crate) fn may_contain_text(data: &[u8]) -> bool {
        // SIMD-accelerated pre-check using memchr to find candidate positions
        // for BT (Begin Text) and Do (XObject invocation) operators.
        // ~50x faster than byte-by-byte scanning for large graphics-heavy pages.
        fn is_boundary(b: u8) -> bool {
            b.is_ascii_whitespace()
                || matches!(
                    b,
                    b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
                )
        }

        // Search for 'B' (BT) and 'D' (Do) candidates using SIMD memchr
        let len = data.len();
        let mut offset = 0;
        while offset + 1 < len {
            // Find next 'B' or 'D' byte
            match memchr::memchr2(b'B', b'D', &data[offset..]) {
                None => return false,
                Some(pos) => {
                    let i = offset + pos;
                    if i + 1 >= len {
                        return false;
                    }
                    // Check for BT operator
                    if data[i] == b'B' && data[i + 1] == b'T' {
                        let before_ok = i == 0 || is_boundary(data[i - 1]);
                        let after_ok = i + 2 >= len || is_boundary(data[i + 2]);
                        if before_ok && after_ok {
                            return true;
                        }
                    }
                    // Check for Do operator
                    if data[i] == b'D' && data[i + 1] == b'o' {
                        let before_ok = i == 0 || is_boundary(data[i - 1]);
                        let after_ok = i + 2 >= len || is_boundary(data[i + 2]);
                        if before_ok && after_ok {
                            return true;
                        }
                    }
                    offset = i + 1;
                }
            }
        }
        false
    }

    /// Check if a page definitely cannot produce any text based on its resources.
    ///
    /// Returns `true` if the page has no `/Font` resources and no Form XObjects
    /// (which could contain nested text). This allows skipping content stream
    /// decompression and parsing entirely for image-only/scanned pages.
    ///
    /// Returns `false` (conservative) if resources can't be inspected.
    pub(super) fn page_cannot_have_text(&self, page_dict: &HashMap<String, Object>) -> bool {
        let resources = match page_dict.get("Resources") {
            Some(r) => {
                if let Some(ref_obj) = r.as_reference() {
                    match self.load_object(ref_obj) {
                        Ok(obj) => obj,
                        Err(_) => return false, // Can't resolve — be conservative
                    }
                } else {
                    r.clone()
                }
            }
            None => return true, // No resources at all → no text possible
        };

        let res_dict = match resources.as_dict() {
            Some(d) => d,
            None => return false,
        };

        // If the page has any /Font resources, it might produce text
        if let Some(font_obj) = res_dict.get("Font") {
            let font_dict = if let Some(ref_obj) = font_obj.as_reference() {
                self.load_object(ref_obj).ok()
            } else {
                Some(font_obj.clone())
            };
            if let Some(fd) = font_dict {
                if let Some(d) = fd.as_dict() {
                    if !d.is_empty() {
                        return false; // Has fonts → might have text
                    }
                }
            }
        }

        // Check XObjects: if any are Form type, they could contain nested text.
        // Uses lightweight is_form_xobject() peek instead of full load_object()
        // to avoid expensive I/O for image-heavy PDFs (e.g., Deutsche: 375MB images).
        if let Some(xobj_obj) = res_dict.get("XObject") {
            let xobj_dict_obj = if let Some(ref_obj) = xobj_obj.as_reference() {
                self.load_object(ref_obj).ok()
            } else {
                Some(xobj_obj.clone())
            };
            if let Some(xobj_dict_resolved) = xobj_dict_obj {
                if let Some(xobj_dict) = xobj_dict_resolved.as_dict() {
                    for xobj_ref in xobj_dict.values() {
                        if let Some(ref_obj) = xobj_ref.as_reference() {
                            // Use lightweight 1KB peek instead of full object load
                            if self.is_form_xobject(ref_obj) {
                                return false; // Form XObject could contain text
                            }
                        } else if let Some(d) = xobj_ref.as_dict() {
                            if d.get("Subtype").and_then(|s| s.as_name()) == Some("Form") {
                                return false;
                            }
                        }
                    }
                }
            }
        }

        // No fonts and no Form XObjects → page is image-only
        true
    }

    /// Assemble the page's text spans via the reading-order
    /// pipeline, classifying each region with the per-class
    /// detectors in [`crate::pipeline::reading_order::detectors`].
    /// Returns the assembled spans plus the detector class that
    /// fired on each region.
    ///
    /// The four detectors handle layout shapes that the plain
    /// y-then-x assembly cannot produce correctly:
    ///
    /// - **DramaticScript**: Macbeth-style speaker-tag layouts —
    ///   row-major join required.
    /// - **DenseSingleLine**: SEC DEF 14A 8pt-body interleave —
    ///   single-row regroup required.
    /// - **SubSuperBaselineReattach**: chemical-formula
    ///   subscripts — baseline reattach required.
    /// - **NarrowTrackedJustified**: stretched justified columns —
    ///   per-line median-gap threshold normalisation required.
    ///
    /// Regions that don't match any specific layout fall through to
    /// `Default` (plain y-then-x assembly within the block).
    ///
    /// Callers can use this as a pre-step before applying their own
    /// assembly logic, or rely on the classified `ReadingOrderClass`
    /// to dispatch their assembly strategy. `extract_text` consumes
    /// this implicitly through `extract_spans` + the existing
    /// `XYCutStrategy`.
    pub fn assemble_text_via_reading_order(
        &self,
        page_index: usize,
    ) -> Result<(
        Vec<crate::layout::TextSpan>,
        crate::pipeline::reading_order::ReadingOrderClass,
    )> {
        if self.is_encrypted_unreadable() {
            log::warn!("PDF is encrypted and could not be decrypted; returning empty text");
            return Ok((
                Vec::new(),
                crate::pipeline::reading_order::ReadingOrderClass::Default,
            ));
        }
        let spans = self.extract_spans(page_index)?;
        // Convert spans to detector input. We only need the geometric
        // signal (x/y/width/font_size), not the full TextSpan
        // semantics.
        let glyphs: Vec<crate::pipeline::reading_order::DetectorGlyph> = spans
            .iter()
            .map(|s| crate::pipeline::reading_order::DetectorGlyph {
                x: s.bbox.x,
                y: s.bbox.y,
                width: s.bbox.width,
                font_size: s.font_size,
                text_len: s.text.chars().count(),
            })
            .collect();
        // Build per-row text strings for DramaticScript detector,
        // together with the leftmost glyph of each row (for the X-
        // consistency check). Group spans by Y (within 0.5 pt),
        // concatenating their texts in the order they appear in
        // `spans` and tracking the smallest X seen per row.
        let mut rows: Vec<(f32, String, crate::pipeline::reading_order::DetectorGlyph)> =
            Vec::new();
        for span in &spans {
            let span_glyph = crate::pipeline::reading_order::DetectorGlyph {
                x: span.bbox.x,
                y: span.bbox.y,
                width: span.bbox.width,
                font_size: span.font_size,
                text_len: span.text.chars().count(),
            };
            let mut placed = false;
            for (y, text, first) in rows.iter_mut() {
                if (*y - span.bbox.y).abs() < 0.5 {
                    text.push(' ');
                    text.push_str(&span.text);
                    if span_glyph.x < first.x {
                        *first = span_glyph;
                    }
                    placed = true;
                    break;
                }
            }
            if !placed {
                rows.push((span.bbox.y, span.text.clone(), span_glyph));
            }
        }
        let row_texts: Vec<&str> = rows.iter().map(|(_, t, _)| t.as_str()).collect();
        let row_first_glyphs: Vec<crate::pipeline::reading_order::DetectorGlyph> =
            rows.iter().map(|(_, _, g)| *g).collect();
        let class =
            crate::pipeline::reading_order::classify_region(&glyphs, &row_first_glyphs, &row_texts);
        Ok((spans, class))
    }

    /// Returns `true` if the page has any text-bearing content (fonts in
    /// resources + at least one `BT`/`Do` operator in the content stream),
    /// `false` if the page is image-only or genuinely empty.
    ///
    /// Callers can route image-only pages to raster rendering instead of
    /// receiving an empty string with no signal.
    ///
    /// Conservative: returns `true` when the page resources can't be
    /// inspected (load error, encrypted-not-authenticated, etc.) so the
    /// caller still attempts extraction.
    ///
    /// # PDF spec basis
    ///
    /// §8.8 (Image XObjects): image-only pages have `/Resources` whose
    /// only `/XObject` entries are `/Subtype /Image` with no `/Font`
    /// resources.
    pub fn has_text_layer(&self, page_index: usize) -> Result<bool> {
        let page = self.get_page(page_index)?;
        let page_dict = page.as_dict().ok_or_else(|| Error::ParseError {
            offset: 0,
            reason: "Page is not a dictionary".to_string(),
        })?;
        if self.page_cannot_have_text(page_dict) {
            return Ok(false);
        }
        // Probe content stream for text-showing operators. If we can't
        // read the content stream, be conservative and say yes (let
        // extraction try).
        match self.get_page_content_data(page_index) {
            Ok(content_data) => Ok(Self::may_contain_text(&content_data)),
            Err(_) => Ok(true),
        }
    }

    /// Returns the document's `/P` permission flags as a `PdfPermissions`
    /// struct if the document is encrypted; `None` otherwise.
    ///
    /// Per PDF spec §7.6.3.2 the `/P` flag is advisory — pdf_oxide
    /// does not enforce restrictions — but callers who want to
    /// enforce them (e.g., refuse copy-protected PDF extraction) can
    /// do so themselves by checking the returned permissions.
    ///
    /// # PDF spec basis
    ///
    /// §7.6.3.2 Table 22 (`/P` Standard Encryption Dictionary entry).
    /// Decoding is implemented in `encryption::permissions::PdfPermissions::from_p_flag`.
    pub fn permissions(&self) -> Option<crate::encryption::PdfPermissions> {
        // ensure_encryption_initialized may fail on malformed Encrypt
        // dicts — that's fine, no permissions surface for those.
        let _ = self.ensure_encryption_initialized();
        let handler = self.encryption_handler.lock_or_recover();
        let handler = handler.as_ref()?;
        Some(crate::encryption::PdfPermissions::from_p_flag(
            handler.raw_permissions(),
        ))
    }
}
