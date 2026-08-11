use super::*;

impl<'doc> TextExtractor<'doc> {
    /// Calculate the average glyph width for a font.
    ///
    /// Computes the mean width of printable ASCII characters (codes 32-126)
    /// in the given font, expressed in thousandths of em.
    ///
    /// # Fallback
    ///
    /// If the font doesn't have a widths array, uses the font's default width.
    ///
    /// # Performance
    ///
    /// This is relatively efficient, typically iterating over 95 ASCII characters.
    /// In practice, most fonts have widths arrays, so this completes quickly.
    #[allow(dead_code)]
    pub(super) fn calculate_average_glyph_width(&self, font: &FontInfo) -> f32 {
        const PRINTABLE_ASCII_START: u32 = 32; // Space
        const PRINTABLE_ASCII_END: u32 = 126; // Tilde

        // If no widths array, use default width
        let Some(ref widths) = font.widths else {
            return font.default_width;
        };

        // We need FirstChar and LastChar to map character codes to width indices
        let Some(first_char) = font.first_char else {
            return font.default_width;
        };
        let Some(last_char) = font.last_char else {
            return font.default_width;
        };

        // Collect widths for all printable ASCII characters
        let mut total_width = 0.0;
        let mut count = 0;

        for char_code in PRINTABLE_ASCII_START..=PRINTABLE_ASCII_END {
            if char_code >= first_char && char_code <= last_char {
                // This character is in the widths array
                let index = (char_code - first_char) as usize;
                if index < widths.len() {
                    total_width += widths[index];
                    count += 1;
                }
            }
        }

        // Return average if we found any widths
        if count > 0 {
            total_width / count as f32
        } else {
            // Fallback if no widths in range
            font.default_width
        }
    }

    /// Add a font to the extractor.
    ///
    /// Fonts must be added before processing content streams that reference them.
    ///
    /// # Arguments
    ///
    /// * `name` - The font resource name (e.g., "F1", "TT1")
    /// * `font` - The font information
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use pdf_oxide::extractors::TextExtractor;
    /// # use pdf_oxide::fonts::FontInfo;
    /// # fn example(font: FontInfo) {
    /// let mut extractor = TextExtractor::new();
    /// extractor.add_font("F1".to_string(), font);
    /// # }
    /// ```
    pub fn add_font(&mut self, name: String, font: FontInfo) {
        self.fonts.insert(name, Arc::new(font));
    }

    /// Add a pre-shared font (Arc-wrapped) to the extractor. Avoids deep cloning.
    pub(crate) fn add_font_shared(&mut self, name: String, font: Arc<FontInfo>) {
        self.fonts.insert(name, font);
    }

    /// Return the current font set for caching purposes.
    pub fn get_font_set(&self) -> Vec<(String, Arc<FontInfo>)> {
        self.fonts
            .iter()
            .map(|(k, v)| (k.clone(), Arc::clone(v)))
            .collect()
    }

    /// Share TrueType cmap tables between fonts with matching base font names.
    /// When a CIDFontType2 Identity-H font has no truetype_cmap, borrow from
    /// another font on the same page with the same base font name (ignoring subset prefix).
    pub fn share_truetype_cmaps(&mut self) {
        // Strip subset prefix (e.g., "QQPMQK+Impact" → "Impact")
        fn strip_subset(name: &str) -> &str {
            if name.len() > 7
                && name.as_bytes()[6] == b'+'
                && name[..6].chars().all(|c| c.is_ascii_uppercase())
            {
                &name[7..]
            } else {
                name
            }
        }

        // First pass: collect the BEST available TrueType cmap for each stripped base font name.
        // When multiple subset variants of the same font exist (e.g., ABCDEF+Arial, GHIJKL+Arial),
        // pick the cmap with the most glyph mappings — it has the best Unicode coverage.
        // On equal coverage, prefer the lexicographically smallest base_font name as a
        // deterministic tie-breaker (HashMap iteration order is randomized per-process).
        let mut best_cmaps: std::collections::HashMap<
            String,
            (crate::fonts::truetype_cmap::TrueTypeCMap, String),
        > = std::collections::HashMap::new();
        for font in self.fonts.values() {
            if let Some(cmap) = font.truetype_cmap() {
                let stripped = strip_subset(&font.base_font).to_string();
                let dominated =
                    best_cmaps
                        .get(&stripped)
                        .is_none_or(|(existing, existing_name)| {
                            match cmap.len().cmp(&existing.len()) {
                                std::cmp::Ordering::Greater => true,
                                std::cmp::Ordering::Equal => font.base_font < *existing_name,
                                std::cmp::Ordering::Less => false,
                            }
                        });
                if dominated {
                    best_cmaps.insert(stripped, (cmap.clone(), font.base_font.clone()));
                }
            }
        }

        if best_cmaps.is_empty() {
            return;
        }

        // Second pass: find CIDFontType2 Identity-H fonts without truetype_cmap
        for font_arc in self.fonts.values_mut() {
            if font_arc.truetype_cmap().is_some() {
                continue;
            }
            // Only target Type0 CIDFontType2 with Identity-H encoding
            if font_arc.subtype != "Type0" {
                continue;
            }
            let is_identity = matches!(&font_arc.encoding, crate::fonts::Encoding::Identity)
                || matches!(&font_arc.encoding, crate::fonts::Encoding::Standard(ref n) if n.contains("Identity"));
            if !is_identity {
                continue;
            }

            let stripped = strip_subset(&font_arc.base_font);
            if let Some((donor_cmap, _)) = best_cmaps.get(stripped) {
                log::info!(
                    "Sharing TrueType cmap ({} entries) to '{}' (Identity-H, no embedded font)",
                    donor_cmap.len(),
                    font_arc.base_font
                );
                // Use Arc::make_mut + set_truetype_cmap for copy-on-write sharing
                Arc::make_mut(font_arc).set_truetype_cmap(Some(donor_cmap.clone()));
            }
        }
    }

    /// Extract text from a content stream.
    ///
    /// Parses the content stream and executes operators to extract positioned
    /// characters with Unicode mappings and font information.
    ///
    /// # Arguments
    ///
    /// * `content_stream` - The raw content stream data (should be decoded first)
    ///
    /// # Returns
    ///
    /// A vector of TextChar structures containing positioned characters.
    ///
    /// # Errors
    ///
    /// Returns an error if the content stream cannot be parsed.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use pdf_oxide::extractors::TextExtractor;
    /// # fn example(content_data: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    /// let mut extractor = TextExtractor::new();
    /// let chars = extractor.extract(content_data)?;
    /// println!("Extracted {} characters", chars.len());
    /// # Ok(())
    /// # }
    /// ```
    /// Extract text as complete spans (PDF spec compliant).
    ///
    /// This is the recommended method for text extraction. It extracts complete
    /// text strings as the PDF provides them via Tj/TJ operators, following the
    /// PDF specification ISO 32000-1:2008.
    ///
    /// # Benefits
    /// - Avoids overlapping character issues
    /// - Preserves PDF's text positioning intent
    /// - More robust for complex layouts
    /// - Matches industry best practices
    ///
    /// # Arguments
    ///
    /// * `content_stream` - The PDF content stream data
    ///
    /// # Returns
    ///
    /// Vector of TextSpan objects in reading order
    pub fn extract_text_spans(&mut self, content_stream: &[u8]) -> Result<Vec<TextSpan>> {
        // Enable span extraction mode
        self.extract_spans = true;
        self.spans.clear();
        self.operators_since_checkpoint = 0;
        self.span_sequence_counter = 0; // Reset sequence counter for this page
                                        // Decide per page whether a whole-body `/PlacedPDF` region must be kept
                                        // rather than suppressed (see `placed_pdf_text_dominates`).
        self.placed_pdf_keep = Self::placed_pdf_text_dominates(content_stream);

        log::debug!("Parsing content stream for text extraction");
        if self.excluded_inks.is_empty() {
            parse_and_execute_text_only(content_stream, |op| self.execute_operator(op))?;
        } else {
            // Ink filtering requires color operators (cs, rg, g, k) which the
            // text-only parser skips. Fall back to the full parser.
            let operators = parse_content_stream(content_stream)?;
            for op in operators {
                self.execute_operator(op)?;
            }
        }

        // Flush any remaining Tj buffer at end of content stream
        self.flush_tj_span_buffer()?;

        // The stride check can only refuse on a multiple of the stride, so a page that
        // ends just past the ceiling would otherwise reach the layout stages unbounded.
        crate::budget::check_page_spans(self.spans.len())?;

        // Detect RTL glyph DRAW DIRECTION on the raw stream order, BEFORE the
        // reading-order sort destroys it (ISO 32000-1 §14.8.2.3.3 method 1).
        self.detect_rtl_draw_direction();

        // Sort spans by reading order (top-to-bottom, left-to-right)
        if log::log_enabled!(log::Level::Debug) {
            let space_spans = self
                .spans
                .iter()
                .filter(|s| s.text.chars().all(|c| c.is_whitespace()))
                .count();
            let offset_semantic = self.spans.iter().filter(|s| s.offset_semantic).count();
            log::debug!(
                "Before sort_spans_by_reading_order(): {} spans total, {} space-only, {} offset_semantic=true",
                self.spans.len(),
                space_spans,
                offset_semantic
            );
        }

        // Snap super/subscript glyph spans onto the baseline of an
        // adjacent base span BEFORE row-aware sorting. PDFs raise
        // or lower the text matrix via the `Ts` (text-rise) operator
        // for super/subscripts (§9.3.7); the rendered glyphs end up
        // at a Y offset of typically 0.3–0.5 × font_size from the
        // baseline. Without the snap, sorting groups all raised
        // glyphs into a separate Y-band above the body, producing
        // output like `"1,2 ★ 3,4 5 / Chibueze, …"` instead of
        // `"Chibueze,1,2★ Caleb,3,4† …"`.
        self.snap_superscript_baselines();

        self.sort_spans_by_reading_order();

        // Deduplicate overlapping spans
        self.deduplicate_overlapping_spans();

        // Merge adjacent spans on the same line to reconstruct complete words
        self.merge_adjacent_spans();

        // Resolve each span's font resource alias (e.g. "F1") to the resolved
        // /BaseFont name (e.g. "Helvetica", "CIDFont+F1") so extract_spans /
        // extract_words / extract_text_lines report the same font name that
        // extract_chars already does (which reads `font.base_font`). Run AFTER
        // merging so span reconstruction still keys off the raw resource alias
        // exactly as before, and it has no effect on the assembled text/md/html
        // output (font names are not emitted there) — only the API surface.
        let resolved_fonts: Vec<Option<String>> = self
            .spans
            .iter()
            .map(|s| {
                self.fonts
                    .get(&s.font_name)
                    .map(|f| f.base_font.clone())
                    .filter(|b| !b.is_empty())
            })
            .collect();
        for (span, resolved) in self.spans.iter_mut().zip(resolved_fonts) {
            if let Some(base_font) = resolved {
                span.font_name = base_font;
            }
        }

        // Attach the §9.10.2 mapping provenance now that each span's font name
        // is finalized: the tier the span's font offered, or `Fallback` when it
        // carries no mapping resource (the text is then a fabricated glyph-index
        // echo, not read from the file). `None` when the font is unresolvable.
        for span in self.spans.iter_mut() {
            span.provenance = self
                .fonts
                .get(span.font_name.as_str())
                .map(|f| f.best_mapping_provenance());
        }

        Ok(std::mem::take(&mut self.spans))
    }

    /// Extract individual characters from a PDF content stream.
    ///
    /// This is a low-level method that extracts characters one by one.
    /// For most use cases, prefer using `extract_text_spans()` which groups
    /// characters into text spans according to PDF semantics.
    pub fn extract(&mut self, content_stream: &[u8]) -> Result<Vec<TextChar>> {
        self.extract_into_self(content_stream)?;
        Ok(self.chars.clone())
    }

    /// Run the character extraction and leave the result in `self.chars`.
    pub(super) fn extract_into_self(&mut self, content_stream: &[u8]) -> Result<()> {
        // Enable character extraction mode
        self.extract_spans = false;
        self.chars.clear();
        self.spans.clear(); // Ensure spans are clear so they don't poison xobject_spans_cache
        self.placed_pdf_keep = Self::placed_pdf_text_dominates(content_stream);

        let operators = if self.excluded_inks.is_empty() {
            parse_content_stream_text_only(content_stream)?
        } else {
            parse_content_stream(content_stream)?
        };
        for op in operators {
            self.execute_operator(op)?;
        }

        // BUG FIX #2: Sort characters by reading order (top-to-bottom, left-to-right)
        // PDF content streams are in rendering order, not reading order.
        // PDF Y coordinates increase upward, so higher Y = top of page.
        // We need to sort by Y descending (top first), then X ascending (left to right).
        self.sort_by_reading_order();

        // BUG FIX #3: Deduplicate overlapping characters
        // Some PDFs render text multiple times (for effects like boldness, shadowing).
        // This causes characters to appear at very close X positions (< 2pt).
        // We deduplicate by keeping only the first character when multiple chars
        // at the same Y position have X positions within 2pt of each other.
        self.deduplicate_overlapping_chars();

        Ok(())
    }

    /// Same extraction as [`Self::extract`], but hands the buffer over instead
    /// of copying it. Every `TextChar` owns a `font_name` String, so `extract`'s
    /// clone re-allocates once per glyph — measurable on long documents. Leaves
    /// `self.chars` empty, so callers that read `char_count`/`chars` afterwards
    /// must keep using [`Self::extract`].
    pub fn extract_owned(&mut self, content_stream: &[u8]) -> Result<Vec<TextChar>> {
        self.extract_into_self(content_stream)?;
        Ok(std::mem::take(&mut self.chars))
    }
}
