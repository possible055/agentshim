use super::*;

impl PdfDocument {
    /// Convert a page to Markdown format.
    ///
    /// Extracts text from the specified page and converts it to Markdown with
    /// optional heading detection and image references.
    ///
    /// # Arguments
    ///
    /// * `page_index` - Zero-based page index
    /// * `options` - Conversion options controlling the output
    ///
    /// # Returns
    ///
    /// A string containing the Markdown representation of the page.
    ///
    /// # Errors
    ///
    /// Returns an error if the page cannot be accessed or conversion fails.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use pdf_oxide::PdfDocument;
    /// use pdf_oxide::converters::ConversionOptions;
    ///
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// let mut doc = PdfDocument::open("paper.pdf")?;
    ///
    /// let options = ConversionOptions {
    ///     detect_headings: true,
    ///     ..Default::default()
    /// };
    /// let markdown = doc.to_markdown(0, &options)?;
    /// println!("{}", markdown);
    /// # Ok(())
    /// # }
    /// ```
    #[allow(clippy::wrong_self_convention)] // Needs mutable access for caching
    pub fn to_markdown(
        &self,
        page_index: usize,
        options: &crate::converters::ConversionOptions,
    ) -> Result<String> {
        self.to_markdown_inner(page_index, options)
    }

    fn to_markdown_inner(
        &self,
        page_index: usize,
        options: &crate::converters::ConversionOptions,
    ) -> Result<String> {
        if self.is_encrypted_unreadable() {
            log::warn!("PDF is encrypted and could not be decrypted; returning empty markdown");
            return Ok(String::new());
        }
        // Apply caller-specified region filters up front so excluded content is
        // gone from EVERY downstream path — tables, headings, reading order
        // (#609: markdown previously ignored `exclude_regions`/`include_region`,
        // which were only honoured by the plain-text path).
        let mut base_spans = Self::apply_region_filters(self.extract_spans(page_index)?, options);

        // WS2.6: strip repeated running headers/footers from untagged output
        // when requested. Opt-in (default off); the /Artifact-tag path already
        // handles tagged PDFs. Computed once per page here — fine for the
        // per-page `to_markdown`; multi-page callers pay O(pages) extra scans.
        if options.strip_running_headers_footers {
            let repeated = self.repeated_running_head_foot(0.6);
            if !repeated.is_empty() {
                let media_h = self
                    .get_page_media_box(page_index)
                    .map(|m| m.3)
                    .unwrap_or(792.0);
                base_spans.retain(|s| !Self::is_running_head_foot(s, media_h, &repeated));
            }
        }

        // Vertical CJK (tategaki, ISO 32000-1 §9.7.4.3): the column-major text is
        // a single flowing paragraph. The horizontal converter pipeline would
        // mis-read its columns (and the spatial table fallback mis-fires on
        // them), so mirror the plain-text path and emit the column-major text.
        if let Some(vertical) = Self::try_assemble_vertical_cjk(&base_spans) {
            return Ok(vertical);
        }

        // Two-column prose (#734) is content-balance-gated to reject real
        // tables, so when it fires suppress the text-only spatial table
        // fallback: a short-cell two-column body must read column-major as
        // prose, not be re-gridded row-wise into a table.
        let two_col_prose = Self::prose_two_column_gutter(&base_spans).is_some();
        let tables = if options.extract_tables && !two_col_prose {
            // text_fallback=true: to_markdown explicitly targets structured output,
            // so we enable the text-only spatial fallback for line-less tables
            // (e.g. sailing-score grids with no ruling lines — issue #486).
            self.extract_page_tables(page_index, &base_spans, options, true)
        } else {
            Vec::new()
        };

        let mut spans = base_spans;
        if options.include_form_fields {
            spans.extend(self.extract_widget_spans(page_index));
        }
        // B1: stitch a full-width line that the producer drew as adjacent
        // fragments straddling the gutter back into one span, so the column
        // reorder below (and the geometric fallback) cannot bucket its halves
        // into different columns and split it mid-word. Mirrors the plain-text
        // path's pre-column `merge_adjacent_spans`; no-op (byte-identical) on any
        // page without such a gutter-crossing run.
        if let Some(coalesced) = Self::coalesce_gutter_crossing_runs(&spans) {
            spans = coalesced;
        }

        // Two-column-prose column-major reorder (#734): same gate + emit as the
        // plain-text path, so a two-column body reads column-by-column rather
        // than interleaving rows. When it fires (untagged pages only), the
        // pipeline preserves this order instead of re-deriving a row-major one;
        // a trustworthy struct tree's mcid order still wins.
        let prose_reordered = Self::reorder_two_column_prose(&mut spans);

        // Publisher-metadata sidebar segregation (RW-1 D3): same gate + emit as
        // the plain-text path; preserve the order so the pipeline does not
        // re-interleave the metadata column into the body (untagged pages).
        let sidebar_reordered = if !prose_reordered {
            if let Some(ordered) = Self::sidebar_body_reading_order(&spans) {
                spans = ordered;
                true
            } else {
                false
            }
        } else {
            false
        };

        let pipeline_config = TextPipelineConfig::from_conversion_options(options);

        let (mcid_order, mcid_to_role, mcid_to_block_id, mcid_preformatted) = {
            // Use structure-tree reading order only when trustworthy (§14.8.2.3.1):
            // honours /MarkInfo /Suspects so markdown stays consistent with
            // extract_text / to_plain_text. (The /Table-element table path in
            // extract_page_tables intentionally keeps its own gate.)
            let cached_tree = self.struct_tree_trustworthy();

            if let Some(ref struct_tree) = cached_tree {
                // Build per-page traversal cache once, then O(1) lookup per page
                if self.structure_content_cache.lock_or_recover().is_none() {
                    let all_content =
                        crate::structure::traverse_structure_tree_all_pages(struct_tree);
                    *self.structure_content_cache.lock_or_recover() = Some(all_content);
                }

                // Extract MCID order AND per-MCID structural role for this page.
                let cached_page_owned = self
                    .structure_content_cache
                    .lock_or_recover()
                    .as_ref()
                    .and_then(|cache| cache.get(&(page_index as u32)))
                    .cloned();
                let cached_page = cached_page_owned.as_deref();

                let order: Vec<u32> = cached_page
                    .map(|content| content.iter().filter_map(|c| c.mcid).collect())
                    .unwrap_or_default();

                let mut role_map: std::collections::HashMap<u32, crate::pipeline::StructRole> =
                    std::collections::HashMap::new();
                let mut block_map: std::collections::HashMap<u32, u32> =
                    std::collections::HashMap::new();
                let mut preformatted_set: std::collections::HashSet<u32> =
                    std::collections::HashSet::new();
                if let Some(content) = cached_page {
                    for item in content {
                        if let Some(mcid) = item.mcid {
                            if item.preformatted {
                                preformatted_set.insert(mcid);
                            }
                            // Heading takes precedence over list role on the
                            // same MCR (a heading-marked-content doesn't
                            // also play a list role in any sane PDF).
                            let role = if let Some(level) = item.heading_level {
                                Some(crate::pipeline::StructRole::Heading(level))
                            } else {
                                item.list_role.map(|lr| match lr {
                                    crate::structure::ListRole::LI => {
                                        crate::pipeline::StructRole::ListItem
                                    }
                                    crate::structure::ListRole::Lbl => {
                                        crate::pipeline::StructRole::ListItemLabel
                                    }
                                    crate::structure::ListRole::LBody => {
                                        crate::pipeline::StructRole::ListItemBody
                                    }
                                })
                            };
                            if let Some(r) = role {
                                // Heading wins over list role on the same MCID.
                                // The comment further up in this function asserts the
                                // precedence ("Heading takes precedence over list role
                                // on the same MCR"); plain `or_insert` would silently
                                // keep whichever role was seen first when the same
                                // MCID appears in two `OrderedContent` entries
                                // (e.g. one referenced from an /H1 sibling and one
                                // from an enclosing /LI in a tagged-tree quirk).
                                use std::collections::hash_map::Entry;
                                match role_map.entry(mcid) {
                                    Entry::Vacant(e) => {
                                        e.insert(r);
                                    }
                                    Entry::Occupied(mut e) => {
                                        let existing = *e.get();
                                        let new_is_heading =
                                            matches!(r, crate::pipeline::StructRole::Heading(_));
                                        let existing_is_heading = matches!(
                                            existing,
                                            crate::pipeline::StructRole::Heading(_)
                                        );
                                        if new_is_heading && !existing_is_heading {
                                            e.insert(r);
                                        }
                                    }
                                }
                            }
                            // First block_id wins per MCID — multiple OrderedContent
                            // entries can share an MCID when the same content is
                            // referenced from sibling structure elements; the first
                            // emit reflects the document order.
                            block_map.entry(mcid).or_insert(item.block_id);
                        }
                    }
                }

                let role_map_opt = if role_map.is_empty() {
                    None
                } else {
                    Some(role_map)
                };
                let block_map_opt = if block_map.is_empty() {
                    None
                } else {
                    Some(block_map)
                };
                let preformatted_opt = if preformatted_set.is_empty() {
                    None
                } else {
                    Some(preformatted_set)
                };

                if !order.is_empty() {
                    log::debug!(
                        "Extracted {} MCIDs ({} typed, {} blocked) from structure tree for page {}",
                        order.len(),
                        role_map_opt.as_ref().map(|m| m.len()).unwrap_or(0),
                        block_map_opt.as_ref().map(|m| m.len()).unwrap_or(0),
                        page_index
                    );
                    (Some(order), role_map_opt, block_map_opt, preformatted_opt)
                } else {
                    log::debug!(
                        "No MCIDs found for page {}, reading order strategy will use geometric fallback",
                        page_index
                    );
                    (None, role_map_opt, block_map_opt, preformatted_opt)
                }
            } else {
                log::debug!(
                    "No structure tree found, reading order strategy will use geometric fallback"
                );
                (None, None, None, None)
            }
        };

        // Step 5: Create pipeline with config
        let pipeline = TextPipeline::with_config(pipeline_config.clone());

        // Step 6: Build reading order context (pass mcid_order if available)
        let mut context = ReadingOrderContext::new()
            .with_page(page_index as u32)
            .with_preserve_input_order(prose_reordered || sidebar_reordered);
        if let Some(order) = mcid_order {
            context = context.with_mcid_order(order);
        }

        // Step 7: Process through pipeline (applies reading order strategy)
        // Repair the cross-span Arabic glyph-interleave defect on the RAW spans,
        // BEFORE the pipeline merges/orders them. The zero-width mark/consonant
        // spans that land inside a word's x-range are only still standalone here;
        // once `pipeline.process` merges adjacent spans the gate can no longer
        // see them (the reason the post-order `OrderedTextSpan` patch never
        // fired). Collapsing each gated pure-RTL line to one visual-order span
        // now lets md/html reconstruct Arabic/Hebrew at glyph fidelity, matching
        // the plain-text path. Returns None — byte-identical — for any page
        // without the interleave, so LTR output is untouched.
        let spans = match Self::merge_interleaved_rtl_lines(&spans) {
            Some(merged) => merged,
            None => spans,
        };
        let mut ordered_spans = pipeline.process(spans, context)?;

        // Annotate ordered spans with the per-MCID structural role
        // paragraph block-id so the markdown converter can emit headings
        // and bullets directly from the source PDF's `/StructTreeRoot`
        // and respect tagged paragraph boundaries even when the
        // geometric inter-paragraph gap is too small for the heuristic
        // (issue #377 D1 + D5 unlock).
        if mcid_to_role.is_some() || mcid_to_block_id.is_some() || mcid_preformatted.is_some() {
            for s in ordered_spans.iter_mut() {
                if let Some(mcid) = s.span.mcid {
                    if let Some(role) = mcid_to_role.as_ref().and_then(|m| m.get(&mcid)) {
                        s.struct_role = Some(*role);
                    }
                    if let Some(bid) = mcid_to_block_id.as_ref().and_then(|m| m.get(&mcid)) {
                        s.block_id = Some(*bid);
                    }
                    if mcid_preformatted
                        .as_ref()
                        .is_some_and(|m| m.contains(&mcid))
                    {
                        s.preformatted = true;
                    }
                }
            }
        }

        // Apply struct-tree-scope /ActualText (ISO 32000-1 §14.9.4):
        // replace covered MCIDs' text with the emission's replacement,
        // suppress non-anchor spans of multi-MCID subtrees. Untagged
        // documents are no-ops.
        self.apply_actualtext_to_ordered_spans(page_index, &mut ordered_spans);

        // Tag spans inside /Link annotations with their URI.
        self.apply_link_annotations_to_ordered_spans(page_index, &mut ordered_spans);

        // Correct right-to-left reading order (Arabic/Hebrew) before the
        // converter emits spans verbatim — the converter pipeline does not
        // reach the plain-text path's RTL passes.
        Self::apply_rtl_logical_order_to_ordered_spans(&mut ordered_spans);

        // Step 8: Use pipeline converter with tables
        let converter = MarkdownOutputConverter::new();
        let markdown = converter.convert_with_tables(&ordered_spans, &tables, &pipeline_config)?;

        Ok(markdown)
    }
}
