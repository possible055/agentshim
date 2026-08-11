use super::parsing::*;
use super::preflight::*;
use super::*;

impl PdfDocument {
    pub(super) fn extract_words_inner(
        &self,
        page_index: usize,
        word_gap_threshold: Option<f32>,
        profile: Option<crate::config::ExtractionProfile>,
        include_artifacts: bool,
    ) -> Result<Vec<crate::layout::Word>> {
        use crate::layout::{clustering, AdaptiveLayoutParams, DocumentProperties, Word};

        // Span source. The default (no profile) flows through the canonical
        // `page_reading_order` helper: tagged → struct tree,
        // untagged → geometric top-to-bottom. The legacy profile path keeps
        // its previous XY-Cut + row-aware-sort behavior pending the planned
        // removal of `profile`.
        let spans: Vec<crate::layout::TextSpan> = match profile {
            Some(p) => {
                use crate::pipeline::reading_order::xycut::XYCutStrategy;
                let config = crate::extractors::TextExtractionConfig::new().with_profile(p);
                let mut s = self.extract_spans_raw_with_extraction_config(page_index, config)?;
                s.sort_by(|a, b| {
                    crate::utils::row_aware_span_cmp(a.bbox.y, a.bbox.x, b.bbox.y, b.bbox.x)
                });
                if !include_artifacts {
                    s.retain(|span| span.artifact_type.is_none());
                }
                let strategy = XYCutStrategy::new();
                strategy
                    .partition_region(&s)
                    .into_iter()
                    .flatten()
                    .collect()
            }
            None => {
                let ordered = if include_artifacts {
                    crate::pipeline::page_reading_order(self, page_index)?
                } else {
                    crate::pipeline::page_reading_order_no_artifacts(self, page_index)?
                };
                ordered.into_iter().map(|os| os.span).collect()
            }
        };
        if spans.is_empty() {
            return Ok(Vec::new());
        }

        // Compute adaptive parameters from all characters for consistent thresholds.
        let media_box = self
            .get_page_media_box(page_index)
            .unwrap_or((0.0, 0.0, 612.0, 792.0));
        let page_bbox =
            crate::geometry::Rect::new(media_box.0, media_box.1, media_box.2, media_box.3);

        // Materialize each span's chars ONCE (to_chars allocates + decodes); the
        // word-clustering loop below reuses chars_per_span instead of calling
        // to_chars a second time per span. Byte-identical, halves to_chars work.
        let mut all_chars: Vec<_> = Vec::new();
        let mut span_char_ranges: Vec<std::ops::Range<usize>> = Vec::with_capacity(spans.len());
        for s in spans.iter() {
            let start = all_chars.len();
            all_chars.extend(s.to_chars());
            span_char_ranges.push(start..all_chars.len());
        }
        if all_chars.is_empty() {
            return Ok(Vec::new());
        }
        let props =
            DocumentProperties::analyze(&all_chars, page_bbox).map_err(Error::LayoutAnalysis)?;
        let mut params = AdaptiveLayoutParams::from_properties(&props);

        // Apply user-provided threshold override
        if let Some(wgt) = word_gap_threshold {
            params.word_gap_threshold = wgt;
        }

        // Walk spans in canonical reading order, clustering chars within each span
        // into words. Since spans come pre-ordered, a flat iteration suffices —
        // no block-by-block partition is needed.
        //
        // Track word indices where the source span had split_boundary_before = true.
        // The post-processing merge must not cross these boundaries (table cells, columns).
        let mut split_boundary_word_indices: std::collections::HashSet<usize> =
            std::collections::HashSet::new();
        // Track word indices produced from spans drawn with a rotated text matrix
        // (rotation_degrees != 0 — figure/axis labels, rotated table headers,
        // vertical margin stamps). Such a run's glyphs advance along a rotated
        // axis, but the span bbox flattens them onto the x-axis (width = Σ glyph
        // advances, height = font). Its flattened bbox therefore overlaps
        // unrelated perpendicular columns, and the reading-order-adjacent word
        // merge below would fuse those columns into one giant token (issue #804:
        // a whole rotated column returned as a 1000+ char "word"). Never merge
        // into or out of a rotated run.
        let mut rotated_word_indices: std::collections::HashSet<usize> =
            std::collections::HashSet::new();
        let mut words = Vec::new();
        for (span_idx, span) in spans.iter().enumerate() {
            let span_chars = &all_chars[span_char_ranges[span_idx].clone()];
            if span_chars.is_empty() {
                continue;
            }

            // Group characters within THIS SPAN. Since PDF spans are often words or line fragments,
            // this is much safer than global character clustering.
            let clusters =
                clustering::cluster_chars_into_words(span_chars, params.word_gap_threshold);

            // Record split boundary: the first word created from this span is a hard
            // boundary when split_boundary_before = true (e.g. table cell boundary).
            let first_word_idx = words.len();
            let is_split_boundary = span.split_boundary_before;
            let is_rotated_run = span.rotation_degrees != 0.0;

            for cluster_indices in clusters {
                let cluster_chars: Vec<_> = cluster_indices
                    .iter()
                    .map(|&i| span_chars[i].clone())
                    .collect();

                let mut current_word_chars = Vec::new();
                for c in cluster_chars {
                    if c.char.is_whitespace() || c.char == '\n' || c.char == '\r' {
                        if !current_word_chars.is_empty() {
                            let mut word = Word::from_chars(current_word_chars);
                            word.sequence = span.sequence;
                            words.push(word);
                            current_word_chars = Vec::new();
                        }
                    } else {
                        current_word_chars.push(c);
                    }
                }
                if !current_word_chars.is_empty() {
                    let mut word = Word::from_chars(current_word_chars);
                    word.sequence = span.sequence;
                    words.push(word);
                }
            }

            // Only mark the boundary if at least one word was created for this span.
            if is_split_boundary && words.len() > first_word_idx {
                split_boundary_word_indices.insert(first_word_idx);
            }
            // Flag every word from a rotated run so the merge below skips it.
            if is_rotated_run {
                rotated_word_indices.extend(first_word_idx..words.len());
            }
        }

        // Post-processing: merge adjacent words whose spans abut or overlap on
        // the same line. PDFs (especially tagged CJK documents) sometimes encode
        // typographically-adjacent glyphs as separate marked-content runs, e.g.
        // "Q" and "（peu/d）" with a gap of -0.18 points. Without merging these
        // remain separate tokens and never match the ground-truth "Q（peu/d）".
        //
        // Merge condition: same line (y_diff ≤ 0.5 × max line height) AND
        // horizontal gap ≤ 0.15 × font_size (same threshold as should_insert_space).
        // Skip merge when the current word index is a split boundary.
        //
        // `gap` has no lower bound above, so a word that BACKTRACKS far behind
        // the previous word's origin also satisfies `gap ≤ 0.15 × font_size`
        // (a large negative number is always ≤ a small positive one). Displayed
        // math draws a fraction's denominator AFTER the relation sign that
        // follows the numerator (`dx/dt = …` → the `=` is emitted, then `dt`
        // starts ~2em further left at a small baseline offset) — this is the
        // exact geometry `assemble_text_from_spans`'s backtrack branch breaks
        // the line on, just reached here through word bboxes instead of span
        // bboxes. Left unguarded, this loop fuses the pair into `"=dt"`, and
        // because the merge is incremental (`prev` grows to the union bbox),
        // a chain of such backtracks collapses into one word spanning an
        // entire equation — the far worse case reported against `main`.
        // Mirror the emitter's guard: a word that starts at-or-left of the
        // previous word's ORIGIN (not just its end), with a real baseline
        // offset and an overlap far beyond ordinary kerning, is a backtrack,
        // not a same-line neighbour — never merge across it. Gated off for
        // RTL text, whose leftward flow is ordinary reading order.
        let mut merged: Vec<Word> = Vec::with_capacity(words.len());
        // RTL-ness of each entry in `merged`, carried alongside it. `looks_rtl`
        // scans a whole string, and `prev` below GROWS by `push_str` on every
        // merge — re-deriving it per iteration made a chain of k merges cost
        // O(k^2) characters, the same blow-up the backtrack guard above exists
        // to prevent. It is an `any()` over the chars, so
        // `looks_rtl(a + b) == looks_rtl(a) || looks_rtl(b)`: maintain it
        // incrementally instead.
        let mut merged_rtl: Vec<bool> = Vec::with_capacity(words.len());
        let mut prev_rotated = false;
        for (idx, word) in words.into_iter().enumerate() {
            let cur_rotated = rotated_word_indices.contains(&idx);
            let word_rtl = crate::text::bidi::looks_rtl(&word.text);
            if !cur_rotated && !prev_rotated && !split_boundary_word_indices.contains(&idx) {
                if let Some(prev) = merged.last_mut() {
                    let gap = word.bbox.x - (prev.bbox.x + prev.bbox.width);
                    let y_diff = (word.bbox.y - prev.bbox.y).abs();
                    let delta_x = word.bbox.x - prev.bbox.x;
                    let line_h = prev.bbox.height.max(word.bbox.height);
                    let font_size = prev.avg_font_size.max(word.avg_font_size).max(1.0);
                    let not_rtl = !merged_rtl.last().copied().unwrap_or(false) && !word_rtl;
                    let is_math_backtrack =
                        y_diff > 1.0 && delta_x <= 0.5 && gap < -font_size && not_rtl;
                    // A LINE WRAP can land at nearly the same y as the line
                    // above it (some producers emit sub-1pt baseline drift
                    // between consecutive lines, so `y_diff > 1.0` above
                    // doesn't always hold), but it always resets x back
                    // toward the page's left margin — an order of magnitude
                    // further than any real same-line construct (ordinary
                    // kerning is near 0; the math backtrack above is ~1-2em).
                    // A multi-em backtrack this large can only be two
                    // different lines, never a genuine adjacency — reject it
                    // regardless of y_diff, or a wrapped line's tail gets
                    // fused onto its own next line's head (e.g. "of whom" +
                    // "tered with books" → "whomteredwithbooks").
                    let is_line_wrap_reset = delta_x < -5.0 * font_size && not_rtl;
                    if y_diff <= line_h * 0.5
                        && gap <= font_size * 0.15
                        && !is_math_backtrack
                        && !is_line_wrap_reset
                    {
                        // Incremental merge — O(k) per merge, O(total_chars) overall.
                        // Avoids the O(n²) clone+from_chars pattern that caused
                        // catastrophic slowdown on TOC dot-leader pages.
                        let prev_n = prev.chars.len() as f32;
                        let word_n = word.chars.len() as f32;
                        prev.bbox = prev.bbox.union(&word.bbox);
                        prev.avg_font_size = (prev.avg_font_size * prev_n
                            + word.avg_font_size * word_n)
                            / (prev_n + word_n);
                        if word_n > prev_n {
                            prev.dominant_font = word.dominant_font;
                        }
                        prev.is_bold |= word.is_bold;
                        prev.is_italic |= word.is_italic;
                        if prev.mcid != word.mcid {
                            prev.mcid = None;
                        }
                        prev.text.push_str(&word.text);
                        prev.chars.extend(word.chars);
                        if let Some(flag) = merged_rtl.last_mut() {
                            *flag |= word_rtl;
                        }
                        continue;
                    }
                }
            }
            merged.push(word);
            merged_rtl.push(word_rtl);
            prev_rotated = cur_rotated;
        }

        Ok(merged)
    }

    /// Extract text lines from a page.
    ///
    /// Groups words into lines based on vertical proximity.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let lines = doc.extract_text_lines(0)?;
    /// for line in lines {
    ///     println!("Line: {} at {:?}", line.text, line.bbox);
    /// }
    /// ```
    pub fn extract_text_lines(&self, page_index: usize) -> Result<Vec<crate::layout::TextLine>> {
        self.extract_text_lines_with_thresholds(page_index, None, None, None)
    }

    /// Extract text lines from a page with optional threshold and profile overrides.
    ///
    /// When thresholds are `None`, adaptive values are computed automatically
    /// from page statistics. Providing values (in PDF points) overrides the
    /// adaptive computation for fine-grained control over segmentation.
    ///
    /// When `profile` is provided, it controls how the underlying text spans are
    /// extracted from the PDF content stream (TJ offset thresholds, word margin
    /// ratios). This affects the raw character data before word/line clustering.
    ///
    /// # Arguments
    ///
    /// * `page_index` - Zero-based page index
    /// * `word_gap_threshold` - Optional override for the horizontal gap (in PDF points)
    ///   used to split characters into words. Smaller values produce more words.
    /// * `line_gap_threshold` - Optional override for the vertical gap (in PDF points)
    ///   used to group words into lines. Smaller values produce more lines.
    /// * `profile` - Optional extraction profile for span-level tuning.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Use adaptive thresholds (default behavior)
    /// let lines = doc.extract_text_lines_with_thresholds(0, None, None, None)?;
    ///
    /// // Tune both thresholds for dense forms
    /// let lines = doc.extract_text_lines_with_thresholds(0, Some(1.5), Some(4.0), None)?;
    /// ```
    pub fn extract_text_lines_with_thresholds(
        &self,
        page_index: usize,
        word_gap_threshold: Option<f32>,
        line_gap_threshold: Option<f32>,
        profile: Option<crate::config::ExtractionProfile>,
    ) -> Result<Vec<crate::layout::TextLine>> {
        // Default: include /Artifact-tagged spans (pre-0.3.42 behavior).
        // Spec-correct variant: [`Self::extract_text_lines_with_thresholds_no_artifacts`].
        self.extract_text_lines_inner(
            page_index,
            word_gap_threshold,
            line_gap_threshold,
            profile,
            true,
        )
    }

    /// Same as [`Self::extract_text_lines_with_thresholds`] but drops spans
    /// tagged as `/Artifact` (running headers/footers, page numbers,
    /// watermarks; ISO 32000-1:2008 §14.8.2.2.1). Spec-correct variant.
    pub fn extract_text_lines_with_thresholds_no_artifacts(
        &self,
        page_index: usize,
        word_gap_threshold: Option<f32>,
        line_gap_threshold: Option<f32>,
        profile: Option<crate::config::ExtractionProfile>,
    ) -> Result<Vec<crate::layout::TextLine>> {
        self.extract_text_lines_inner(
            page_index,
            word_gap_threshold,
            line_gap_threshold,
            profile,
            false,
        )
    }

    pub(super) fn extract_text_lines_inner(
        &self,
        page_index: usize,
        word_gap_threshold: Option<f32>,
        line_gap_threshold: Option<f32>,
        profile: Option<crate::config::ExtractionProfile>,
        include_artifacts: bool,
    ) -> Result<Vec<crate::layout::TextLine>> {
        use crate::layout::{clustering, AdaptiveLayoutParams, DocumentProperties, TextLine, Word};

        // Span source. Default (no profile) → canonical `page_reading_order`
        // helper. Legacy profile path keeps XY-Cut + row-aware
        // sort pending the planned removal of `profile`.
        let spans: Vec<crate::layout::TextSpan> = match profile {
            Some(p) => {
                use crate::pipeline::reading_order::xycut::XYCutStrategy;
                let config = crate::extractors::TextExtractionConfig::new().with_profile(p);
                let mut s = self.extract_spans_raw_with_extraction_config(page_index, config)?;
                s.sort_by(|a, b| {
                    crate::utils::row_aware_span_cmp(a.bbox.y, a.bbox.x, b.bbox.y, b.bbox.x)
                });
                if !include_artifacts {
                    s.retain(|span| span.artifact_type.is_none());
                }
                let strategy = XYCutStrategy::new();
                strategy
                    .partition_region(&s)
                    .into_iter()
                    .flatten()
                    .collect()
            }
            None => {
                let ordered = if include_artifacts {
                    crate::pipeline::page_reading_order(self, page_index)?
                } else {
                    crate::pipeline::page_reading_order_no_artifacts(self, page_index)?
                };
                ordered.into_iter().map(|os| os.span).collect()
            }
        };
        if spans.is_empty() {
            return Ok(Vec::new());
        }

        // Compute adaptive parameters
        let media_box = self
            .get_page_media_box(page_index)
            .unwrap_or((0.0, 0.0, 612.0, 792.0));
        let page_bbox =
            crate::geometry::Rect::new(media_box.0, media_box.1, media_box.2, media_box.3);

        // Materialize each span's chars once (see extract_text_as_words).
        let mut all_chars: Vec<_> = Vec::new();
        let mut span_char_ranges: Vec<std::ops::Range<usize>> = Vec::with_capacity(spans.len());
        for s in spans.iter() {
            let start = all_chars.len();
            all_chars.extend(s.to_chars());
            span_char_ranges.push(start..all_chars.len());
        }
        let props =
            DocumentProperties::analyze(&all_chars, page_bbox).map_err(Error::LayoutAnalysis)?;
        let mut params = AdaptiveLayoutParams::from_properties(&props);

        // Apply user-provided threshold overrides
        if let Some(wgt) = word_gap_threshold {
            params.word_gap_threshold = wgt;
        }
        if let Some(lgt) = line_gap_threshold {
            params.line_gap_threshold = lgt;
        }

        // Walk spans in canonical reading order, clustering chars → words.
        // No block partition; spans are already pre-ordered.
        //
        // `word_rot_run` maps each word to the index of the rotated span it came
        // from (`None` for horizontal spans). A rotated run's glyphs advance
        // along a rotated axis but the span bbox flattens them onto the x-axis,
        // so the flattened y-band line clustering below would fuse the run with
        // its perpendicular neighbours into one giant line (#804). Rotated runs
        // are therefore lifted out and each emitted as its own line.
        let mut words: Vec<Word> = Vec::new();
        let mut word_rot_run: Vec<Option<usize>> = Vec::new();
        for (span_idx, span) in spans.iter().enumerate() {
            let span_chars = &all_chars[span_char_ranges[span_idx].clone()];
            if span_chars.is_empty() {
                continue;
            }
            let rot_run = (span.rotation_degrees != 0.0).then_some(span_idx);

            let clusters =
                clustering::cluster_chars_into_words(span_chars, params.word_gap_threshold);
            for cluster_indices in clusters {
                let cluster_chars: Vec<_> = cluster_indices
                    .iter()
                    .map(|&i| span_chars[i].clone())
                    .collect();
                let mut current_word_chars = Vec::new();
                for c in cluster_chars {
                    if c.char.is_whitespace() || c.char == '\n' || c.char == '\r' {
                        if !current_word_chars.is_empty() {
                            let mut word = Word::from_chars(current_word_chars);
                            word.sequence = span.sequence;
                            words.push(word);
                            word_rot_run.push(rot_run);
                            current_word_chars = Vec::new();
                        }
                    } else {
                        current_word_chars.push(c);
                    }
                }
                if !current_word_chars.is_empty() {
                    let mut word = Word::from_chars(current_word_chars);
                    word.sequence = span.sequence;
                    words.push(word);
                    word_rot_run.push(rot_run);
                }
            }
        }

        if words.is_empty() {
            return Ok(Vec::new());
        }

        // Fast path (byte-identical): no rotated runs on the page → cluster every
        // word by global y-tolerance exactly as before. Same-y words merge into
        // the same line regardless of source span (span ordering already handled
        // the multi-column / structure-tree sequencing upstream).
        if word_rot_run.iter().all(Option::is_none) {
            let line_clusters =
                clustering::cluster_words_into_lines(&words, params.line_gap_threshold);
            let mut all_lines = Vec::new();
            for cluster_indices in line_clusters {
                let cluster_words: Vec<_> =
                    cluster_indices.iter().map(|&i| words[i].clone()).collect();
                all_lines.push(TextLine::new(cluster_words));
            }
            return Ok(all_lines);
        }

        // Rotated-content page: y-band-cluster the horizontal words only, and
        // emit each rotated run as its own line, then restore reading order by
        // the span sequence of each line's first word.
        let horizontal: Vec<Word> = words
            .iter()
            .zip(word_rot_run.iter())
            .filter(|(_, r)| r.is_none())
            .map(|(w, _)| w.clone())
            .collect();
        let mut lines: Vec<Vec<Word>> = Vec::new();
        if !horizontal.is_empty() {
            for cluster_indices in
                clustering::cluster_words_into_lines(&horizontal, params.line_gap_threshold)
            {
                lines.push(
                    cluster_indices
                        .iter()
                        .map(|&i| horizontal[i].clone())
                        .collect(),
                );
            }
        }
        // One line per rotated run (contiguous words sharing the same span index).
        let mut run_start = 0;
        while run_start < words.len() {
            match word_rot_run[run_start] {
                None => run_start += 1,
                Some(run_id) => {
                    let mut run_end = run_start + 1;
                    while run_end < words.len() && word_rot_run[run_end] == Some(run_id) {
                        run_end += 1;
                    }
                    lines.push(words[run_start..run_end].to_vec());
                    run_start = run_end;
                }
            }
        }
        // Reading order: sort lines by the span sequence of their first word
        // (stable so intra-line order is preserved).
        lines.sort_by_key(|line| line.first().map(|w| w.sequence).unwrap_or(usize::MAX));

        Ok(lines.into_iter().map(TextLine::new).collect())
    }

    /// Get the raw content stream data for a page.
    ///
    /// This returns the decoded content stream bytes for the specified page.
    /// The content stream contains PDF operators that define the page's appearance.
    pub fn get_page_content_data(&self, page_index: usize) -> Result<Vec<u8>> {
        Ok((*self.cached_page_content(page_index)?).clone())
    }

    /// Shared, cached content-stream bytes for a page — the same data
    /// [`Self::get_page_content_data`] returns, minus the copy. Extraction
    /// only ever reads the bytes, and a single `extract_words` page touches
    /// this twice (once for spans, once for chars), so handing back the `Arc`
    /// avoids copying the decompressed stream on every call.
    pub(super) fn cached_page_content(&self, page_index: usize) -> Result<std::sync::Arc<Vec<u8>>> {
        {
            let mut cache = self.page_content_cache.lock_or_recover();
            if let Some(data) = cache.get(&page_index) {
                return Ok(std::sync::Arc::clone(data));
            }
        }

        // Ensure encryption is initialized if needed
        self.ensure_encryption_initialized()?;

        // Get page object
        let page = self.get_page(page_index)?;
        let page_dict = page.as_dict().ok_or_else(|| Error::ParseError {
            offset: 0,
            reason: "Page is not a dictionary".to_string(),
        })?;

        // Get content stream(s) — Contents is optional per ISO 32000-1:2008 Table 30
        let contents_ref = match page_dict.get("Contents") {
            Some(Object::Null) | None => {
                log::debug!("Page {} has no /Contents (blank page)", page_index);
                return Ok(std::sync::Arc::new(Vec::new()));
            }
            Some(c) => c,
        };

        // Contents can be either a single stream, an array of streams, or a direct stream object
        let content_data = if let Some(contents_ref_val) = contents_ref.as_reference() {
            // Contents is a reference - it could point to either a Stream or an Array
            let contents = self.load_object(contents_ref_val)?;

            // Check if the loaded object is an Array (indirect array)
            if let Some(contents_array) = contents.as_array() {
                // The reference pointed to an array of streams
                let mut combined = Vec::new();

                for content_item in contents_array.iter() {
                    if matches!(content_item, Object::Null) {
                        continue;
                    }
                    match (|| -> Result<Vec<u8>> {
                        if let Some(ref_val) = content_item.as_reference() {
                            let content_obj = self.load_object(ref_val)?;
                            self.decode_stream_with_encryption(&content_obj, ref_val)
                        } else {
                            content_item.decode_stream_data()
                        }
                    })() {
                        Ok(decoded) => {
                            combined.extend_from_slice(&decoded);
                            combined.push(b'\n');
                        }
                        Err(e) => {
                            // Skipping is right for a malformed element, but a budget
                            // refusal or a cancellation is not a property of the element:
                            // swallowing it would report a truncated page as a complete
                            // one and hide the limit that actually stopped the work.
                            if matches!(e, Error::ResourceLimit { .. } | Error::Cancelled) {
                                return Err(e);
                            }
                            log::warn!(
                                "Failed to decode content stream element on page {}: {}, skipping",
                                page_index,
                                e
                            );
                        }
                    }
                }

                combined
            } else {
                // The reference pointed to a single stream
                // Decode with encryption support, using the object reference
                self.decode_stream_with_encryption(&contents, contents_ref_val)?
            }
        } else if let Some(contents_array) = contents_ref.as_array() {
            // Array of streams - can be references or direct objects
            let mut combined = Vec::new();

            for content_item in contents_array.iter() {
                if matches!(content_item, Object::Null) {
                    continue;
                }
                match (|| -> Result<Vec<u8>> {
                    if let Some(ref_val) = content_item.as_reference() {
                        let content_obj = self.load_object(ref_val)?;
                        self.decode_stream_with_encryption(&content_obj, ref_val)
                    } else {
                        content_item.decode_stream_data()
                    }
                })() {
                    Ok(decoded) => {
                        combined.extend_from_slice(&decoded);
                        combined.push(b'\n');
                    }
                    Err(e) => {
                        if matches!(e, Error::ResourceLimit { .. } | Error::Cancelled) {
                            return Err(e);
                        }
                        log::warn!(
                            "Failed to decode content stream element on page {}: {}, skipping",
                            page_index,
                            e
                        );
                    }
                }
            }

            combined
        } else {
            // Direct stream object (rare but possible)
            // For direct objects, use regular decoding (no encryption key)
            contents_ref.decode_stream_data()?
        };

        log::debug!(
            "Retrieved {} bytes of content data for page {}: {:?}",
            content_data.len(),
            page_index,
            String::from_utf8_lossy(&content_data)
        );

        let content_data = std::sync::Arc::new(content_data);
        self.page_content_cache
            .lock_or_recover()
            .insert(page_index, std::sync::Arc::clone(&content_data));

        Ok(content_data)
    }
}
