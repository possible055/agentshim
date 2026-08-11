use super::parsing::*;
use super::preflight::*;
use super::*;

impl PdfDocument {
    pub(super) fn ensure_running_artifact_signatures(
        &self,
    ) -> Result<std::sync::Arc<std::collections::HashMap<String, usize>>> {
        {
            let guard = self.running_artifact_signatures.lock_or_recover();
            if let Some(ref map) = *guard {
                // Shared by reference: this runs once per page and the map is
                // document-wide, so cloning it here scaled with page count.
                return Ok(std::sync::Arc::clone(map));
            }
        }
        let page_count = self.page_count()?;
        if page_count < 2 {
            let empty = std::sync::Arc::new(std::collections::HashMap::new());
            *self.running_artifact_signatures.lock_or_recover() =
                Some(std::sync::Arc::clone(&empty));
            return Ok(empty);
        }

        // (count of distinct pages seeing the signature, first page it appeared on).
        // `first_seen_any` tracks the earliest page a signature appeared on
        // regardless of body-content — so if the cover page is all-chrome
        // (no body text), it still registers as "first seen" and gets its
        // title kept by the per-page mark_running_artifact_spans exemption.
        let mut occurrences: std::collections::HashMap<String, (usize, usize)> =
            std::collections::HashMap::new();
        let mut first_seen_any: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        // Track distinct literal texts per signature. A signature whose digits
        // are stable across every page (i.e. the literal text never changes) is
        // NOT a page-number-containing header — it is substantive content that
        // happens to repeat. Only suppress signatures where the literal text
        // varies (at least two distinct forms) meaning digits change per page.
        let mut literal_variants: std::collections::HashMap<
            String,
            std::collections::HashSet<String>,
        > = std::collections::HashMap::new();
        for pi in 0..page_count {
            let spans = match self.extract_spans_raw(pi) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let (page_width, page_height) = match self.get_page_media_box(pi) {
                Ok((_, _, w, h)) if h > 0.0 => (w, h),
                _ => continue,
            };
            let vertical = Self::page_is_vertical(&spans);
            // Require that the page has CONTENT outside the chrome band(s)
            // before counting band spans as candidate artifacts. Otherwise, a
            // page consisting only of a title near the top would have its own
            // title classified as a "running header" across all pages. (For
            // horizontal pages this is identical to the prior top/bottom test.)
            let has_body_content = spans.iter().any(|s| {
                !s.text.trim().is_empty()
                    && !Self::in_chrome_band(&s.bbox, page_width, page_height, vertical)
            });
            // Collect per-page unique signatures from the chrome bands.
            // Runs even when there's no body content so `first_seen_any`
            // registers the cover page even if it's all-chrome.
            let mut seen_this_page: std::collections::HashMap<String, String> =
                std::collections::HashMap::new();
            for s in spans.iter() {
                let trimmed = s.text.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if !Self::in_chrome_band(&s.bbox, page_width, page_height, vertical) {
                    continue;
                }
                let sig = Self::normalize_artifact_signature(trimmed);
                if sig.is_empty() || sig.chars().count() < 2 {
                    continue;
                }
                seen_this_page
                    .entry(sig)
                    .or_insert_with(|| trimmed.to_string());
            }
            // Track first-seen across ALL pages (even body-content-skipped)
            for sig in seen_this_page.keys() {
                first_seen_any.entry(sig.clone()).or_insert(pi);
            }
            // Track literal variants — if the literal text for a signature
            // differs across pages, the digits are varying (page numbers).
            for (sig, literal) in &seen_this_page {
                literal_variants
                    .entry(sig.clone())
                    .or_default()
                    .insert(literal.clone());
            }
            if !has_body_content {
                continue;
            }
            // Count only pages with body content for the recurrence threshold
            for sig in seen_this_page.into_keys() {
                let entry = occurrences.entry(sig).or_insert((0, pi));
                entry.0 += 1;
                if pi < entry.1 {
                    entry.1 = pi;
                }
            }
        }
        let threshold = (page_count as f32 * 0.5).ceil() as usize;
        let signatures: std::collections::HashMap<String, usize> = occurrences
            .into_iter()
            .filter(|(sig, (count, _))| {
                let variants = literal_variants.get(sig).map(|s| s.len()).unwrap_or(0);
                // Varying-literal path (page numbers / dates): the digits change per
                // page. Recurs on >=50% of body pages.
                if *count >= threshold.max(2) && variants >= 2 {
                    return true;
                }
                // Item 6B (M5): CONSTANT-literal pagination/citation (DOI, volume/
                // article, journal URL + digit). The literal never changes, so the
                // varying-literal gate above misses it. Require a STRICTER >=60%
                // recurrence AND the narrow citation/URL shape gate, so substantive
                // repeated content (facility names, titles) is never suppressed.
                let strict = (page_count as f32 * 0.6).ceil() as usize;
                if *count >= strict.max(2)
                    && variants < 2
                    && literal_variants
                        .get(sig)
                        .and_then(|s| s.iter().next())
                        .is_some_and(|lit| Self::looks_like_stable_pagination(lit))
                {
                    return true;
                }
                false
            })
            .map(|(sig, _)| {
                // Use the earliest page the signature appeared on — which
                // may be a body-content-skipped cover page that `occurrences`
                // didn't count toward the threshold but `first_seen_any` did.
                let first = first_seen_any.get(&sig).copied().unwrap_or(0);
                (sig, first)
            })
            .collect();
        let signatures = std::sync::Arc::new(signatures);
        *self.running_artifact_signatures.lock_or_recover() =
            Some(std::sync::Arc::clone(&signatures));
        Ok(signatures)
    }

    /// Mark spans near the top/bottom of the page whose normalized text
    /// matches a cached running-artifact signature by setting
    /// `artifact_type` to Pagination.
    /// #553: a bare page number (e.g. " 1 ", "12") varies per page, so it
    /// never matches a repeated-text signature and leaks into the body. Treat
    /// a short pure-digit token (1..=9999) as a page-number candidate — only
    /// applied inside the top/bottom margin band by the caller, so ordinary
    /// numerals in body text are never affected.
    pub(super) fn is_bare_page_number_text(trimmed: &str) -> bool {
        // Bound by character count, not byte length: non-Latin folio digits are
        // 2–3 UTF-8 bytes each, so a byte cap would reject "۱۲۳" outright.
        if trimmed.is_empty() || trimmed.chars().count() > 4 {
            return false;
        }
        // Fold the (script-aware) digits to a value directly; `parse::<u32>`
        // and `char::to_digit` are ASCII-only and reject non-Latin folios.
        let mut value: u32 = 0;
        for c in trimmed.chars() {
            match Self::folio_digit_value(c) {
                Some(d) => value = value * 10 + d,
                None => return false,
            }
        }
        (1..=9999).contains(&value)
    }

    pub(super) fn mark_running_artifact_spans(
        &self,
        page_index: usize,
        spans: &mut [crate::layout::TextSpan],
    ) -> Result<()> {
        let (_, _, page_width, page_height) = match self.get_page_media_box(page_index) {
            Ok(mb) => mb,
            Err(_) => return Ok(()),
        };
        if page_height <= 0.0 {
            return Ok(());
        }
        let vertical = Self::page_is_vertical(spans);
        // Snapshot baselines of every non-blank span, so the bare-page-number
        // rule can require a candidate to stand ALONE on its line (#553): a
        // digit adjacent to other text — e.g. the "8" in "8th" — is content,
        // not a page number.
        let occupied_baselines: Vec<f32> = spans
            .iter()
            .filter(|s| !s.text.trim().is_empty())
            .map(|s| s.bbox.y)
            .collect();
        // Signature set may be empty (no repeated headers/footers); the
        // bare-page-number rule below still runs.
        let signatures = self.ensure_running_artifact_signatures()?;
        for s in spans.iter_mut() {
            if s.artifact_type.is_some() {
                continue;
            }
            if !Self::in_chrome_band(&s.bbox, page_width, page_height, vertical) {
                continue;
            }
            let trimmed = s.text.trim();
            if trimmed.is_empty() {
                continue;
            }
            // #553: standalone page-number chrome in the margin band — only
            // when the digit is ISOLATED on its line (no other text span
            // within ~one line height), so digits embedded in words/runs are
            // never dropped.
            if Self::is_bare_page_number_text(trimmed) {
                let line_tol = s.font_size.max(6.0);
                let on_line = occupied_baselines
                    .iter()
                    .filter(|&&oy| (oy - s.bbox.y).abs() < line_tol)
                    .count();
                if on_line <= 1 {
                    s.artifact_type = Some(crate::extractors::text::ArtifactType::Pagination(
                        crate::extractors::text::PaginationSubtype::PageNumber,
                    ));
                }
                continue;
            }
            if signatures.is_empty() {
                continue;
            }
            let sig = Self::normalize_artifact_signature(trimmed);
            if let Some(&first_seen_on) = signatures.get(&sig) {
                // Keep the first appearance — it's usually the document
                // cover-page title that got classified as chrome only
                // because later pages repeat it as a running header (B3).
                if page_index == first_seen_on {
                    continue;
                }
                s.artifact_type = Some(crate::extractors::text::ArtifactType::Pagination(
                    crate::extractors::text::PaginationSubtype::Other,
                ));
            }
        }
        Ok(())
    }

    /// Internal helper: extract raw (unsorted) text spans from a page.
    ///
    /// This is the common extraction logic shared by `extract_spans`
    /// `extract_spans_with_reading_order`. Spans are returned without any
    /// sorting or erase-region filtering applied.
    pub(super) fn extract_spans_raw(
        &self,
        page_index: usize,
    ) -> Result<Vec<crate::layout::TextSpan>> {
        self.extract_spans_raw_with_extraction_config(
            page_index,
            crate::extractors::TextExtractionConfig::default(),
        )
    }

    /// Internal helper: extract raw text spans using a specific extraction config.
    ///
    /// This allows callers to provide a [`TextExtractionConfig`] (optionally
    /// configured with an [`ExtractionProfile`]) to control TJ offset thresholds
    /// and word boundary detection during span extraction.
    pub(super) fn extract_spans_raw_with_extraction_config(
        &self,
        page_index: usize,
        config: crate::extractors::TextExtractionConfig,
    ) -> Result<Vec<crate::layout::TextSpan>> {
        self.extract_spans_impl(page_index, config, HashSet::new(), HashSet::new())
    }

    pub(super) fn extract_spans_raw_filtered(
        &self,
        page_index: usize,
        excluded_layers: HashSet<String>,
        excluded_inks: HashSet<String>,
    ) -> Result<Vec<crate::layout::TextSpan>> {
        self.extract_spans_impl(
            page_index,
            crate::extractors::TextExtractionConfig::default(),
            excluded_layers,
            excluded_inks,
        )
    }

    pub(super) fn extract_spans_impl(
        &self,
        page_index: usize,
        config: crate::extractors::TextExtractionConfig,
        excluded_layers: HashSet<String>,
        excluded_inks: HashSet<String>,
    ) -> Result<Vec<crate::layout::TextSpan>> {
        if self.is_encrypted_unreadable() {
            log::warn!("PDF is encrypted and could not be decrypted; returning no spans");
            return Ok(Vec::new());
        }
        use crate::extractors::TextExtractor;

        let page = self.get_page(page_index)?;
        let page_dict = page.as_dict().ok_or_else(|| Error::ParseError {
            offset: 0,
            reason: "Page is not a dictionary".to_string(),
        })?;

        if self.page_cannot_have_text(page_dict) {
            return Ok(Vec::new());
        }

        let content_data = match self.get_page_content_data(page_index) {
            Ok(data) => data,
            Err(e) => {
                // Reporting a limit or a cancellation as "no content" tells the caller
                // the page is empty, which is worse than telling them it failed: they
                // stop looking. Degrading stays correct for a malformed stream.
                if matches!(e, Error::ResourceLimit { .. } | Error::Cancelled) {
                    return Err(e);
                }
                log::warn!(
                    "Failed to decode content stream for page {}: {}, returning empty",
                    page_index,
                    e
                );
                return Ok(Vec::new());
            }
        };

        if !Self::may_contain_text(&content_data) {
            return Ok(Vec::new());
        }

        let mut extractor = TextExtractor::with_config(config);
        // Stamp the page index so spans carry McidScope::Page(page_index)
        // by default; Form XObject Do invocations push their own scope
        // on top of the stack inside the extractor.
        extractor.set_page_index(page_index as u32);
        if !excluded_layers.is_empty() {
            extractor.set_excluded_layers(excluded_layers);
        }
        if !excluded_inks.is_empty() {
            extractor.set_excluded_inks(excluded_inks);
        }
        if let Some(resources) = page_dict.get("Resources") {
            extractor.set_resources(resources.clone());
            extractor.set_document(self);
            if let Err(e) = self.load_fonts(resources, &mut extractor) {
                log::warn!(
                    "Failed to load fonts for page {}: {}, continuing with defaults",
                    page_index,
                    e
                );
            }
        }

        let spans = extractor.extract_text_spans(&content_data)?;
        // Drain MCIDs whose in-stream /ActualText was applied during
        // extraction and stash on the document so the struct-tree-
        // scope applier honours MC-scope-wins precedence (§14.9.4).
        //
        // The per-page entry is REPLACED, not extended: every
        // `extract_spans_impl` call is a self-contained per-page
        // extraction and its own MC-scope detections must be
        // authoritative. Accumulating would make stale results from
        // an earlier filter-set leak into a later, differently-
        // filtered call.
        let mc_set = extractor.take_mc_actualtext_mcids();
        let mut guard = self.mc_actualtext_mcids.lock_or_recover();
        if mc_set.is_empty() {
            guard.remove(&page_index);
        } else {
            guard.insert(page_index, mc_set);
        }
        Ok(spans)
    }

    /// Extract text from a page, excluding content from specified layers and inks.
    ///
    /// Uses the same full text assembly pipeline as [`extract_text`](Self::extract_text)
    /// (structure-tree ordering, table detection, column detection), but with
    /// layer/ink-excluded spans removed before assembly.
    ///
    /// **Ink filtering note:** For DeviceN color spaces, text is suppressed if
    /// ANY ink in the DeviceN array matches an excluded ink name. Tint values
    /// are not evaluated — this is an all-or-nothing match.
    ///
    /// # Arguments
    ///
    /// * `page_index` - Zero-based page index
    /// * `excluded_layers` - OCG layer names to suppress (empty = no layer filtering)
    /// * `excluded_inks` - Separation/DeviceN ink names to suppress (empty = no ink filtering)
    pub fn extract_text_filtered(
        &self,
        page_index: usize,
        excluded_layers: HashSet<String>,
        excluded_inks: HashSet<String>,
    ) -> Result<String> {
        if excluded_layers.is_empty() && excluded_inks.is_empty() {
            return self.extract_text(page_index);
        }

        let spans = self.extract_spans_filtered(page_index, excluded_layers, excluded_inks)?;
        let options = crate::converters::ConversionOptions {
            extract_tables: true,
            ..Default::default()
        };
        self.assemble_text_from_spans(page_index, spans, &options)
    }

    /// Extract text from a region of a page with layer/ink filtering applied.
    ///
    /// Composes [`Self::extract_text_filtered`] with [`Self::extract_text_in_rect`]: spans
    /// are filtered by layer/ink first, then by region, then assembled via
    /// the full text pipeline (structure-tree ordering, table detection,
    /// column detection, whitespace + line breaks).
    pub fn extract_text_filtered_in_rect(
        &self,
        page_index: usize,
        excluded_layers: HashSet<String>,
        excluded_inks: HashSet<String>,
        region: crate::geometry::Rect,
        mode: crate::layout::RectFilterMode,
    ) -> Result<String> {
        let spans = if excluded_layers.is_empty() && excluded_inks.is_empty() {
            self.extract_spans(page_index)?
        } else {
            self.extract_spans_filtered(page_index, excluded_layers, excluded_inks)?
        };
        let options = crate::converters::ConversionOptions {
            extract_tables: true,
            include_region: Some((region, mode)),
            ..Default::default()
        };
        self.assemble_text_from_spans(page_index, spans, &options)
    }

    /// Geometric `ColumnAware` (XY-cut) span ordering. Shared by the
    /// `ColumnAware` and `Structure` reading-order branches (the latter uses it
    /// as its baseline and its tiebreak for unstructured spans).
    pub(super) fn order_spans_column_aware(
        &self,
        spans: Vec<crate::layout::TextSpan>,
        page_index: usize,
    ) -> Result<Vec<crate::layout::TextSpan>> {
        use crate::pipeline::reading_order::{
            ReadingOrderContext as ROContext, ReadingOrderStrategy, XYCutStrategy,
        };
        let strategy = XYCutStrategy::new();
        let context = ROContext::new().with_page(page_index as u32);
        let ordered = strategy.apply(spans, &context)?;
        Ok(ordered.into_iter().map(|o| o.span).collect())
    }

    /// Extract text spans from a page using a specified reading order strategy.
    ///
    /// This method extracts text spans identically to [`extract_spans`](Self::extract_spans),
    /// then applies the chosen reading order strategy to sort them.
    ///
    /// # Arguments
    ///
    /// * `page_index` - Zero-based page index
    /// * `reading_order` - The reading order strategy to apply
    ///
    /// # Returns
    ///
    /// Vector of TextSpan objects sorted according to the chosen reading order.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use pdf_oxide::document::{PdfDocument, ReadingOrder};
    /// # fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let mut doc = PdfDocument::open("two_column.pdf")?;
    /// let spans = doc.extract_spans_with_reading_order(0, ReadingOrder::ColumnAware)?;
    /// for span in spans {
    ///     println!("{}", span.text);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn extract_spans_with_reading_order(
        &self,
        page_index: usize,
        reading_order: ReadingOrder,
    ) -> Result<Vec<crate::layout::TextSpan>> {
        self.extract_spans_filtered_with_reading_order(
            page_index,
            reading_order,
            HashSet::new(),
            HashSet::new(),
        )
    }

    /// Extract positioned spans in a reading order, excluding optional-content
    /// layers and/or Separation/DeviceN inks.
    ///
    /// This is [`extract_spans_with_reading_order`](Self::extract_spans_with_reading_order)
    /// and [`extract_text_filtered`](Self::extract_text_filtered) combined: the
    /// former cannot filter, and the latter returns assembled text rather than
    /// positioned spans. A consumer that lays spans out itself - an HTML/XML
    /// emitter placing each span at its own rectangle - needs both at once.
    ///
    /// The motivating case is render/extract parity. `render_page` honours the
    /// document's default configuration `/OCProperties/D`, but span extraction
    /// treats everything as visible unless the caller names layers (see the
    /// `optional_content` module note). Passing
    /// `optional_content::compute_default_off_ocgs(&doc)` as `excluded_layers`
    /// makes extraction agree with what the page actually displays - without it,
    /// a default-OFF layer holding a copy of the page contributes a SECOND copy
    /// of every word.
    ///
    /// Empty sets are exactly equivalent to the unfiltered call, so this is a
    /// superset of the existing API and costs nothing when no filtering is asked
    /// for.
    ///
    /// # Arguments
    ///
    /// * `page_index` - Zero-based page index
    /// * `reading_order` - The reading order strategy to apply
    /// * `excluded_layers` - OCG layer names to suppress (empty = no filtering)
    /// * `excluded_inks` - Separation/DeviceN ink names to suppress (empty = none)
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use pdf_oxide::document::{PdfDocument, ReadingOrder};
    /// # use pdf_oxide::optional_content;
    /// # fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let doc = PdfDocument::open("layered.pdf")?;
    /// // Agree with what the page displays: drop the default-off layers.
    /// let hidden = optional_content::compute_default_off_ocgs(&doc);
    /// let spans = doc.extract_spans_filtered_with_reading_order(
    ///     0,
    ///     ReadingOrder::ColumnAware,
    ///     hidden,
    ///     Default::default(),
    /// )?;
    /// for span in spans {
    ///     println!("{}", span.text);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn extract_spans_filtered_with_reading_order(
        &self,
        page_index: usize,
        reading_order: ReadingOrder,
        excluded_layers: HashSet<String>,
        excluded_inks: HashSet<String>,
    ) -> Result<Vec<crate::layout::TextSpan>> {
        // Extract raw spans using the common extraction logic. The unfiltered
        // path is kept verbatim for empty sets so existing callers are unchanged.
        let mut spans = if excluded_layers.is_empty() && excluded_inks.is_empty() {
            self.extract_spans_raw(page_index)?
        } else {
            self.extract_spans_raw_filtered(page_index, excluded_layers, excluded_inks)?
        };

        // Drop text lying entirely off the MediaBox - a doc that reuses one big
        // Form XObject across pages relies on the `W n` clip to hide the off-page
        // portion, which the raw extractor does not honour. `extract_spans` applies
        // this via `postprocess_spans`; the reading-order path must too, or it
        // emits every page's worth of spans (measured: a stats report emitted a
        // chart's full hidden data table, ~5x the visible label count).
        self.drop_offpage_spans(page_index, &mut spans);

        // Apply reading order strategy
        match reading_order {
            ReadingOrder::TopToBottom => {
                // Row-aware sort: Y-band descending, then X ascending.
                spans.sort_by(|a, b| {
                    crate::utils::row_aware_span_cmp(a.bbox.y, a.bbox.x, b.bbox.y, b.bbox.x)
                });
            }
            ReadingOrder::ColumnAware => {
                spans = self.order_spans_column_aware(spans, page_index)?;
            }
            ReadingOrder::Structure => {
                // Geometric order is the baseline. The structure tree then fixes ONLY
                // the spans it can fix unambiguously: TABLE cells.
                //
                // A geometric XY-cut reads a wide table column-major and drops cells;
                // the structure tree's pre-order traversal gives the authoritative
                // row-major order (§14.8.2.3). But applying that traversal to the WHOLE
                // page also reorders flowing prose - where the tree's section order can
                // legitimately differ from visual order (it de-prioritises page
                // artifacts, for one) - which is a change, not an improvement. So table
                // content is reordered IN PLACE by structure rank while every non-table
                // span keeps its geometric position.
                let mut ordered = self.order_spans_column_aware(spans, page_index)?;
                if let Some(tree) = self.struct_tree_trustworthy() {
                    // Populate the structure-content cache, then read it (it carries the
                    // per-MCID `in_table` flag the mcid-order list does not).
                    let _ = self.cached_mcid_order_for_page(&tree, page_index as u32);
                    let content: Vec<crate::structure::OrderedContent> = self
                        .structure_content_cache
                        .lock_or_recover()
                        .as_ref()
                        .and_then(|c| c.get(&(page_index as u32)))
                        .cloned()
                        .unwrap_or_default();

                    let page_scope = crate::structure::McidScope::Page(page_index as u32);
                    // Structure rank for TABLE MCIDs only.
                    let mut table_rank: HashMap<(crate::structure::McidScope, u32), usize> =
                        HashMap::new();
                    for (i, c) in content.iter().enumerate() {
                        if let (true, Some(m)) = (c.in_table, c.mcid) {
                            let scope = c.mcid_scope.clone().unwrap_or_else(|| page_scope.clone());
                            table_rank.entry((scope, m)).or_insert(i);
                        }
                    }

                    if !table_rank.is_empty() {
                        let key_of = |s: &crate::layout::TextSpan| {
                            s.mcid.and_then(|m| {
                                let scope =
                                    s.mcid_scope.clone().unwrap_or_else(|| page_scope.clone());
                                table_rank
                                    .get(&(scope, m))
                                    .or_else(|| table_rank.get(&(page_scope.clone(), m)))
                                    .copied()
                            })
                        };
                        // The slots the table spans occupy in geometric order, and the
                        // table spans themselves.
                        let mut slots: Vec<usize> = Vec::new();
                        let mut cells: Vec<(usize, crate::layout::TextSpan)> = Vec::new();
                        for (idx, s) in ordered.iter().enumerate() {
                            if let Some(r) = key_of(s) {
                                slots.push(idx);
                                cells.push((r, s.clone()));
                            }
                        }
                        // Re-fill those exact slots with the cells in structure
                        // (row-major) order. Non-table spans never move.
                        cells.sort_by_key(|(r, _)| *r);
                        for (slot, (_, cell)) in slots.into_iter().zip(cells) {
                            ordered[slot] = cell;
                        }
                    }
                }
                spans = ordered;
            }
        }

        // Apply struct-tree-scope /ActualText (ISO 32000-1 §14.9.4).
        self.apply_actualtext_to_spans(page_index, &mut spans);

        Ok(spans)
    }

    /// Extract complete page text data in a single call.
    ///
    /// Returns a [`PageText`](crate::layout::text_block::PageText) containing spans in reading order, per-character
    /// data derived from those spans (using font-metric widths when available),
    /// and the page dimensions. Uses the default `TopToBottom` reading order.
    ///
    /// # Arguments
    ///
    /// * `page_index` - Zero-based page index
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use pdf_oxide::document::PdfDocument;
    /// # fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let mut doc = PdfDocument::open("example.pdf")?;
    /// let page_text = doc.extract_page_text(0)?;
    /// println!("Page {}x{} pt", page_text.page_width, page_text.page_height);
    /// println!("{} spans, {} chars", page_text.spans.len(), page_text.chars.len());
    /// # Ok(())
    /// # }
    /// ```
    pub fn extract_page_text(&self, page_index: usize) -> Result<crate::layout::PageText> {
        self.extract_page_text_with_options(page_index, ReadingOrder::default())
    }
}
