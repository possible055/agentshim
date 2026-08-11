use super::parsing::*;
use super::preflight::*;
use super::*;

impl PdfDocument {
    pub(super) fn assemble_text_from_spans(
        &self,
        page_index: usize,
        base_spans: Vec<crate::layout::TextSpan>,
        options: &crate::converters::ConversionOptions,
    ) -> Result<String> {
        if self.is_encrypted_unreadable() {
            log::warn!("PDF is encrypted and could not be decrypted; returning empty text");
            return Ok(String::new());
        }

        let base_spans = Self::apply_region_filters(base_spans, options);
        // Struct-tree-scope `/ActualText` is applied per branch below
        // — the structure-order assembler handles it natively via the
        // per-page action map, and the geometric branch applies the
        // raw-span applier on its own input. Pre-applying here would
        // double-process: the structure-order path would see already-
        // mutated spans and lose run-position information, dropping
        // sibling MCIDs of a nested scope (CRITICAL-1 shape).

        // Structure tree: use it for reading order only when it is trustworthy
        // per the shared predicate (§14.8.2.3.1) — the document is /Marked or
        // the catalog references a /StructTreeRoot (PDF 1.4 documents such as
        // hello_structure.pdf predate /MarkInfo but are still tagged, §14.7.1),
        // AND /MarkInfo /Suspects is not true. Suspect documents fall through to
        // the geometric `else` arm below, the spec-correct behaviour.
        let cached_tree = self.struct_tree_trustworthy();
        let widget_spans = self.extract_widget_spans(page_index);

        // Table detection uses base spans only (no widget spans).
        let tables = if options.extract_tables {
            // text_fallback=false: extract_text preserves the pre-v0.3.47 behaviour
            // where line-less pages return no tables. Only the structured-output
            // converters (to_markdown, to_html) opt in to text-only spatial fallback.
            self.extract_page_tables(page_index, &base_spans, options, false)
        } else {
            Vec::new()
        };

        let mut all_spans = base_spans;
        all_spans.extend(widget_spans);

        if all_spans.is_empty() {
            let page = self.get_page(page_index)?;
            let page_dict = page.as_dict().ok_or_else(|| Error::ParseError {
                offset: 0,
                reason: "Page is not a dictionary".to_string(),
            })?;
            let no_content_text = if self.page_cannot_have_text(page_dict) {
                true
            } else {
                // Also check content stream for BT/Do operators (SIMD-fast scan).
                match self.get_page_content_data(page_index) {
                    Ok(ref content_data) => !Self::may_contain_text(content_data),
                    Err(_) => false, // Can't read content stream — be conservative
                }
            };
            if no_content_text {
                let mut text = String::new();
                self.append_non_widget_annotation_text(page_index, &mut text);
                return Ok(text);
            }
        }

        let text = if let Some(ref struct_tree) = cached_tree {
            // Build per-page traversal cache once, then O(1) lookup per page.
            if self.structure_content_cache.lock_or_recover().is_none() {
                let all_content = crate::structure::traverse_structure_tree_all_pages(struct_tree);
                *self.structure_content_cache.lock_or_recover() = Some(all_content);
            }
            self.extract_text_structure_order_cached_with_spans(
                page_index,
                all_spans,
                options.include_artifacts,
            )?
        } else {
            // Untagged or Suspects=true PDF: use page content
            // (geometric) order. Apply struct-tree-scope `/ActualText`
            // here — the structure-order assembler above handles it
            // natively for the trustworthy branch. Suspects=true
            // documents still get their producer-supplied replacement
            // because `actualtext_index()` is decoupled from
            // `struct_tree_marked` (§14.9.4 is content replacement,
            // not a reading-order signal).
            let mut spans = all_spans;
            self.apply_actualtext_to_spans(page_index, &mut spans);

            // Exclude spans that are inside detected tables, BUT
            // preserve multi-row-spanning label columns.
            // The spatial table extractor clusters data cells into
            // table cells but does NOT emit the sparse label column
            // that sits vertically centred within each multi-row data
            // block (common on CJK lab-report reference tables like
            // WS/T 779). Those labels would otherwise be dropped
            // entirely from the output: the retain below would remove
            // them because their bbox is inside the table,
            // `table.render_text()` would not re-emit them because the
            // extractor never captured them as cells. Before running
            // the retain filter we identify these rowspan labels (same
            // heuristic `reorder_rowspan_labels` uses) and keep them in
            // the span list so `reorder_rowspan_labels` below can
            // promote them to the top of their row block.
            if !tables.is_empty() {
                // Absorb floating-point accumulation error in the
                // difference between a span's directly-computed
                // bbox.right (origin + width, small accumulation)
                // and a table bbox.right (min/max reduction across
                // many cell edges, larger accumulation). Without
                // this slack, a span whose real geometry is inside
                // the table by construction but whose f32 right-edge
                // exceeds the table's f32 right-edge by ~0.01–0.05pt
                // gets wrongly kept in the flow stream, producing
                // duplicated output. 0.1pt is well below any
                // visually meaningful PDF layout distance.
                const RETAIN_TOLERANCE: f32 = 0.1;

                // Build the set of cell text strings that every detected
                // table will render via `table.render_text()`. Labels
                // whose exact text already appears as a cell in some
                // table are already covered by the inline-table flush
                // below, so we must NOT also preserve them in the flow
                // span list (it would produce duplicate output).
                let mut table_cell_texts: std::collections::HashSet<String> =
                    std::collections::HashSet::new();
                for t in &tables {
                    for row in &t.rows {
                        for cell in &row.cells {
                            let trimmed = cell.text.trim();
                            if !trimmed.is_empty() {
                                table_cell_texts.insert(trimmed.to_string());
                            }
                        }
                    }
                }

                // For tagged PDFs, collect the MCIDs that are actually owned by
                // table cells. When a span's MCID is NOT in this set, the span is
                // NOT part of the table even if it lies inside the table's bbox
                // (e.g. a paragraph physically adjacent to a table that was tagged
                // as a sibling <P> element, not as a <TD>). Filtering such spans
                // by bbox alone would silently drop real content.
                // Falls back to bbox-only filtering when no MCIDs are present
                // (untagged PDFs or spatial-detection tables).
                let table_cell_mcids: HashSet<u32> = tables
                    .iter()
                    .flat_map(|t| {
                        t.rows
                            .iter()
                            .flat_map(|r| r.cells.iter().flat_map(|c| c.mcids.iter().copied()))
                    })
                    .collect();
                // Flatten every cell bbox once and index it into coarse y-bands,
                // so the per-span containment test below scans only the cells in
                // the span's y-band instead of every cell on the page (was
                // O(spans x cells) on untagged table pages). A cell that contains
                // a span necessarily shares the span's y-band, so this is
                // byte-identical to the full scan.
                let cell_bboxes: Vec<crate::geometry::Rect> = tables
                    .iter()
                    .flat_map(|t| {
                        t.rows
                            .iter()
                            .flat_map(|r| r.cells.iter().filter_map(|c| c.bbox))
                    })
                    .collect();
                const CELL_Y_BIN: f32 = 18.0;
                let cell_bin = |y: f32| (y / CELL_Y_BIN).floor() as i32;
                let mut cell_y_index: std::collections::HashMap<i32, Vec<usize>> =
                    std::collections::HashMap::new();
                for (ci, b) in cell_bboxes.iter().enumerate() {
                    for bin in cell_bin(b.y)..=cell_bin(b.y + b.height) {
                        cell_y_index.entry(bin).or_default().push(ci);
                    }
                }
                // Returns true when span should be removed from the flow because
                // it is owned by a table cell (will be re-emitted by render_text).
                let span_in_table = |s: &crate::layout::TextSpan| -> bool {
                    if !table_cell_mcids.is_empty() {
                        if let Some(mcid) = s.mcid {
                            // Tagged PDF: MCID decides ownership precisely.
                            return table_cell_mcids.contains(&mcid);
                        }
                        // Tagged PDF but span has no MCID (widget/annotation):
                        // keep in flow — better to duplicate than to silently drop.
                        return false;
                    }
                    // Untagged PDF or no MCIDs in any cell: cell-bbox-based filter.
                    // Using per-cell bboxes (rather than the coarser table bbox) prevents
                    // dropping paragraph spans that lie inside the table's outer bounding
                    // box but were not captured as table cells by the spatial detector.
                    // Probe only the cells in the span's y-band (±1 bin guards the
                    // containment tolerance). Equivalent to scanning every cell.
                    let slo = cell_bin(s.bbox.y) - 1;
                    let shi = cell_bin(s.bbox.y + s.bbox.height) + 1;
                    let in_cell = (slo..=shi).any(|bin| {
                        cell_y_index.get(&bin).is_some_and(|cands| {
                            cands.iter().any(|&ci| {
                                Self::contains_rect_with_tolerance(
                                    &cell_bboxes[ci],
                                    &s.bbox,
                                    RETAIN_TOLERANCE,
                                )
                            })
                        })
                    });
                    if in_cell {
                        return true;
                    }
                    // Fallback: text-based match. The bbox check above uses
                    // a tight 0.1pt tolerance and rejects spans whose font
                    // ascent extends slightly above the cell's ink box (issue
                    // 484: "FY 15 1st Q TTL" labels in JAL traffic table —
                    // span height = font_size = 10.7pt, but cell bbox height
                    // = 15.96pt covers two ink rows so the label glyphs reach
                    // ~0.4pt above the cell's top edge). When a span's
                    // trimmed text exactly matches some cell's text, the cell
                    // already owns it — keeping it in flow would duplicate.
                    let trimmed = s.text.trim();
                    if trimmed.is_empty() {
                        return false;
                    }
                    if !table_cell_texts.contains(trimmed) {
                        return false;
                    }
                    // Require spatial proximity: the span must lie inside
                    // some table's outer bbox so we don't drop body text that
                    // coincidentally matches a cell's text elsewhere on the page.
                    tables.iter().any(|t| {
                        t.bbox.is_some_and(|tb| {
                            let cx = s.bbox.x + s.bbox.width / 2.0;
                            let cy = s.bbox.y + s.bbox.height / 2.0;
                            cx >= tb.x - RETAIN_TOLERANCE
                                && cx <= tb.x + tb.width + RETAIN_TOLERANCE
                                && cy >= tb.y - RETAIN_TOLERANCE
                                && cy <= tb.y + tb.height + RETAIN_TOLERANCE
                        })
                    })
                };

                let preserved_label_indices: std::collections::HashSet<usize> =
                    Self::identify_multi_row_labels(&spans)
                        .into_iter()
                        .filter(|&idx| {
                            // Only preserve labels whose text is NOT
                            // already emitted by any table's
                            // `render_text()`. This is what makes the
                            // #329 fix safe on pages where the spatial
                            // extractor captured the sparse label
                            // column as cells — we let the table
                            // render them and drop them from flow.
                            // On pages like WS/T 779 where the label
                            // column is a genuine multi-row-spanning
                            // column that the extractor did NOT
                            // capture, the set is empty and every
                            // identified label stays in flow where
                            // `reorder_rowspan_labels` below can
                            // promote it.
                            let t = spans[idx].text.trim();
                            !t.is_empty() && !table_cell_texts.contains(t)
                        })
                        .collect();

                if preserved_label_indices.is_empty() {
                    spans.retain(|s| !span_in_table(s));
                } else {
                    let kept: Vec<crate::layout::TextSpan> = spans
                        .drain(..)
                        .enumerate()
                        .filter_map(|(i, s)| {
                            if !span_in_table(&s) || preserved_label_indices.contains(&i) {
                                Some(s)
                            } else {
                                None
                            }
                        })
                        .collect();
                    spans = kept;
                }
            }

            // Row-aware ordering: quantize Y into bands and sort band-
            // descending, then X ascending within a band. Strict Y sorting
            // would interleave cells from the same tabular row whose Y
            // values differ by typographic jitter (common in CJK layouts,
            // superscripts, and centered multi-line labels).
            //
            // Skip for multi-column pages: extract_spans() already applied
            // XY-cut column ordering. Re-sorting with row-aware would interleave
            // left/right columns line-by-line, splicing words from adjacent
            // columns into each other. A topological block order (a precede
            // relation over text blocks) handles genuine multi-region pages (a
            // two-column body/footer, a sidebar beside the body) that a flat
            // row-aware (y,x) sort interleaves. The gate (substantial, text-dense,
            // dominant side-by-side blocks) rejects single-column pages, tables,
            // TOCs and forms; it de-interleaves real two-column bodies and
            // sidebar+body layouts. topological_block_order runs unconditionally;
            // it self-gates to None unless the page has dominant side-by-side
            // text-dense blocks (see its side_by_side gate), so single-column,
            // table, and TOC pages are byte-identical.
            //
            // Item 2 (M2): first lift a narrow, sparse, body-aligned marginalia
            // rail (manuscript line numbers / a folio rail) OUT of the body, so
            // the column dispatch below sees a clean body. A rail otherwise
            // injects a spurious second corridor (disqualifying prose detection)
            // and a sparse block (defeating the topological side-by-side gate),
            // and a flat (y,x) sort then weaves its numerals into the prose. The
            // rail is re-appended at the end of the reading order after the
            // ladder, before the artifact retain. No-op (None) on ordinary pages
            // → byte-identical.
            let marginalia_trailing: Vec<crate::layout::TextSpan> =
                if let Some(idx) = Self::lift_marginalia_column(&spans) {
                    let idxset: std::collections::HashSet<usize> = idx.into_iter().collect();
                    let mut keep = Vec::with_capacity(spans.len());
                    let mut marg = Vec::new();
                    for (i, s) in std::mem::take(&mut spans).into_iter().enumerate() {
                        if idxset.contains(&i) {
                            marg.push(s);
                        } else {
                            keep.push(s);
                        }
                    }
                    marg.sort_by(|a, b| {
                        crate::utils::row_aware_span_cmp(a.bbox.y, a.bbox.x, b.bbox.y, b.bbox.x)
                    });
                    spans = keep;
                    marg
                } else {
                    Vec::new()
                };

            let mut topo_applied = false;
            if let Some(reordered) = Self::topological_block_order(&spans) {
                spans = reordered;
                topo_applied = true;
            }
            if topo_applied {
                // Topological block order replaced the row-aware sort.
            } else if let Some(ordered) = Self::sidebar_body_reading_order(&spans) {
                // Narrow metadata SIDEBAR + wide body (e.g. an MDPI first page
                // whose left rail carries Citation:/Received:/Accepted:/Copyright
                // furniture). The row-aware (y,x) sort otherwise threads that rail
                // INTO the body paragraphs at matching Y-bands; segregating it so
                // each region reads contiguously matches a block-based extractor
                // and stops the rail-into-body interleave. Already used by the
                // md/html/structured paths; this wires the SAME ordering into the
                // plain-text path. Tightly gated (≥30 spans, narrow sidebar with
                // ≥2 furniture labels), so it is a no-op (None) on ordinary pages.
                spans = ordered;
            } else if let Some(gutter_x) = Self::prose_two_column_gutter(&spans)
                .or_else(|| Self::classifier_column_gutter(&spans))
            {
                // Genuine two-column prose (content-balance gated — forms /
                // TOC / tables / figures are rejected), OR a ragged
                // reference list / dense results body that the clean corridor
                // sweep and `is_multi_column_page` MISS but the per-column region
                // classifier confirms (`classifier_column_gutter`). Both read
                // column-major with band separation: full-width rows (titles,
                // mid-body section headings, footers — spans crossing the gutter)
                // are emitted at their vertical position, between the column runs
                // around them, so they are never split across the gutter
                // (§14.8.3). This branch is tried BEFORE the single-column
                // row-aware path so a 2-column reference page (which fails
                // `is_multi_column_page`) is reordered instead of interleaved;
                // both gutter detectors return None on single-column pages, which
                // then fall through to the row-aware branch unchanged.
                Self::reorder_column_major_with_bands(&mut spans, gutter_x);
                // NB: do NOT run reorder_same_line_runs here. The column emit
                // already orders each column by (y desc, x asc); a same-line
                // X-sort would re-merge vertically-adjacent lines whenever the
                // body leading (e.g. ~9pt) is tighter than same_line_threshold
                // (min_fs·1.2 ≈ 10.8pt), pulling a new left-margin reference
                // ahead of the previous reference's indented continuation
                // (bibliography interleave) or shattering wrapped hyphenated
                // lines in dense two-column bodies.
            } else if !Self::is_multi_column_page(&spans)
                || (!tables.is_empty() && Self::multicol_signal_is_tabular(&spans, &tables))
            {
                // Either a genuine single-column page, OR a single-column page
                // whose only multi-column geometric signal comes from a TABLE
                // (a data grid whose column-aligned cells trip
                // `is_multi_column_page`). In the latter case the genuine
                // two-column branches (topological / prose-gutter / classifier)
                // above all declined, so the page is NOT a two-column body; the
                // multi-column false positive is purely tabular. The correct
                // reading order is then the row-aware (y desc, x asc) band sort
                // — it linearises both the surrounding prose AND the table rows.
                // Without it the page keeps raw content-stream order, which on
                // these journal pages interleaves the table's column-major cell
                // stream INTO the prose paragraph (PMC8078162 §3.1). Gated on a
                // detected table whose region accounts for the multi-column
                // signal (`multicol_signal_is_tabular`), so genuine two-column
                // pages — which the column branches catch first, and which carry
                // no page-dominating table — are unaffected.
                spans.sort_by(|a, b| {
                    let cmp =
                        crate::utils::row_aware_span_cmp(a.bbox.y, a.bbox.x, b.bbox.y, b.bbox.x);
                    if cmp != std::cmp::Ordering::Equal {
                        return cmp;
                    }
                    a.sequence.cmp(&b.sequence)
                });

                // Promote multi-row-spanning labels (sparse-column spans
                // vertically centred across several dense-column data rows)
                // to sort at the top of their row block.
                Self::reorder_rowspan_labels(&mut spans);

                // Restore intra-line reading order after the row-aware band sort.
                // Off-baseline glyphs (e.g. superscripts/subscripts) can land in
                // adjacent bands and be emitted out of X order; fix that per line.
                Self::reorder_same_line_runs(&mut spans);
            }

            // Re-append the lifted marginalia rail at the end of the body
            // reading order (Item 2 / M2). Done before the artifact retain so
            // any artifact-marked rail spans are still dropped.
            spans.extend(marginalia_trailing);

            // Drop content marked /Artifact (PDF Spec ISO 32000-1:2008
            // §14.8.2.2 — headers, footers, page numbers, decorations) —
            // unless the caller opted in via `options.include_artifacts`
            // (default true). Untagged-PDF running-header detection
            // runs at document level and feeds the same artifact_type flag.
            if !options.include_artifacts {
                spans.retain(|s| s.artifact_type.is_none());
            }

            // RTL correction
            Self::reverse_rtl_visual_order_runs(&mut spans);

            // Filter out invalid spans
            spans.retain(|s| {
                s.bbox.x.is_finite()
                    && s.bbox.y.is_finite()
                    && s.bbox.width.is_finite()
                    && s.bbox.height.is_finite()
                    && s.font_size.is_finite()
            });

            // Merge subscript/superscript spans into their base spans so that
            // tokens like "k1" and "k2" appear as single words rather than
            // as isolated fragments interleaved with other spans (pdfa_004).
            Self::merge_sub_superscript_spans(&mut spans);

            // Inline table insertion.
            //
            // Tables were previously rendered in a single block appended
            // at the end of the page text, after all flow spans. That
            // matches how `extract_text` historically worked but it means
            // tabular content appears far away from the prose that
            // surrounds it in reading order — on product data sheets
            // like ORAFOL 5900 the "Physical and Chemical Properties"
            // label/value rows showed up 20+ lines below the section
            // they belong to, which the reporter of #315 perceived as
            // the content being dropped entirely.
            //
            // Instead, maintain a sorted queue of tables keyed by their
            // top-Y (the larger Y coordinate of the table's bbox, per PDF
            // user-space conventions where Y grows upward). As we walk
            // the flow spans in row-aware reading order, whenever the
            // next span's top-Y falls below the top-Y of the queue's
            // leading table, we flush that table's rendered text at
            // that point, then continue. A final pass at the end emits
            // any tables whose top-Y is below all remaining spans (or
            // that have no flow spans at all).
            //
            // Tables are emitted at most once regardless of how many
            // spans sit above them, preserving existing behaviour
            // semantics while inlining the rendering at its spatial
            // reading-order position.
            let mut pending_tables: Vec<(f32, &crate::structure::table_extractor::Table)> = tables
                .iter()
                .filter_map(|t| t.bbox.map(|b| (b.y + b.height, t)))
                .collect();
            // Sort descending by top-Y so `pop()` returns the next table
            // to emit in reading order (larger Y first).
            pending_tables.sort_by(|(a, _), (b, _)| crate::utils::safe_float_cmp(*b, *a));

            let flush_table =
                |text: &mut String, table: &crate::structure::table_extractor::Table| {
                    if !text.is_empty() && !text.ends_with('\n') {
                        text.push('\n');
                    }
                    text.push('\n');
                    text.push_str(&table.render_text());
                    if !text.ends_with('\n') {
                        text.push('\n');
                    }
                };

            let mut text = String::with_capacity(spans.len() * 20);
            let mut prev_span: Option<TextSpan> = None;

            for span in &spans {
                // Flush any tables that sit above this span in PDF
                // reading order (their top-Y is greater than or equal
                // to the span's top-Y, meaning they should appear first).
                while let Some(&(table_top_y, table)) = pending_tables.last() {
                    let span_top_y = span.bbox.y + span.bbox.height;
                    if table_top_y >= span_top_y {
                        flush_table(&mut text, table);
                        pending_tables.pop();
                        // Reset prev_span so the flow-text glue logic
                        // doesn't try to stitch the table's rendered
                        // block together with the next flow span.
                        prev_span = None;
                    } else {
                        break;
                    }
                }

                if let Some(prev) = &prev_span {
                    let prev_end_x = prev.bbox.x + prev.bbox.width;
                    let span_end_x = span.bbox.x + span.bbox.width;
                    // Containment check: skip a span only if it is geometrically
                    // contained within the previous span AND has identical text.
                    // Without the text comparison, distinct lines that happen to
                    // overlap spatially (e.g., due to small Tm-scaled offsets)
                    // would be silently dropped.
                    let y_same = (prev.bbox.y - span.bbox.y).abs() < 2.0;
                    if y_same
                        && span.bbox.x >= prev.bbox.x - 0.5
                        && span_end_x <= prev_end_x + 0.5
                        && span.text == prev.text
                    {
                        continue;
                    }

                    let y_diff = (prev.bbox.y - span.bbox.y).abs();
                    let gap = span.bbox.x - prev_end_x;
                    let delta_x = span.bbox.x - prev.bbox.x;

                    // Korean mid-eojeol soft wrap (SEG-KO): keep a Hangul word whole
                    // when it wrapped mid-syllable. The wrap surfaces either as a
                    // y-line break OR (when the two halves share a baseline band) as a
                    // large backward X jump, so it is gated at each break site below.
                    let hangul_midword_wrap = Self::hangul_midword_line_wrap(&text, prev, span);
                    if y_diff > Self::same_line_threshold(prev, span) {
                        let font_size = prev.font_size.max(span.font_size).max(10.0);
                        let line_height = font_size * 1.2;
                        let num_breaks = (y_diff / line_height).round() as usize;
                        if !(hangul_midword_wrap && num_breaks == 1) {
                            for _ in 0..num_breaks.clamp(1, 3) {
                                text.push('\n');
                            }
                        }
                    } else if gap < -1.0 {
                        let fs = span.font_size.max(prev.font_size).max(6.0);
                        if gap < -(fs * 20.0) {
                            if !hangul_midword_wrap && !text.ends_with('\n') {
                                text.push('\n');
                            }
                        } else if delta_x < -fs * 3.0 {
                            // Same visual line, but the current span starts well to the LEFT of the
                            // previous span's start — i.e., the upstream ordering is non-monotonic in X.
                            // This commonly occurs with multi-column layouts or XY-cut artifacts where
                            // spans from different visual rows fall within the same Y tolerance band
                            // (see `same_line_threshold`).
                            //
                            // Without inserting a separator, these spans would be concatenated
                            // (e.g. `instancesinstancesinstances` from adjacent table headers).
                            // Treat this backward X jump as a logical break and emit a newline.
                            if !text.ends_with('\n') {
                                text.push('\n');
                            }
                        } else if gap < -fs * 3.0 && y_diff > fs * 0.5 && delta_x <= fs * 0.5 {
                            // Soft-wrapped next line (carriage return). Three
                            // signals must coincide so inline math/super-scripts
                            // and chart labels are NOT mistaken for a wrap:
                            //   • `gap < -fs*3` — the previous span ended far to
                            //     the RIGHT of where this one starts, i.e. the
                            //     line filled and wrapped (adjacent math glyphs
                            //     have a near-zero gap, so they are excluded);
                            //   • `y_diff > fs*0.5` — a real baseline drop (a
                            //     super/sub-script shift is smaller);
                            //   • `delta_x <= fs*0.5` — it returned to ~the line's
                            //     left margin.
                            // Its leading sits UNDER `same_line_threshold`
                            // (single-spaced body at ~1.0 em vs the 1.2 em
                            // threshold) so the y-newline branch above never
                            // fired, leaving the line-final and line-initial words
                            // glued together. A line break encodes no space (ISO
                            // 32000 §9.4.2 — positioning is geometric); synthesize
                            // one as a newline, which the downstream join /
                            // `normalize_text` collapses to a space and
                            // de-hyphenates.
                            if !hangul_midword_wrap && !text.ends_with('\n') {
                                text.push('\n');
                            }
                        } else if y_diff > 1.0
                            && delta_x <= 0.5
                            && gap < -fs
                            && !prev.rtl_draw_logical
                            && !span.rtl_draw_logical
                            && !crate::text::bidi::looks_rtl(&prev.text)
                            && !crate::text::bidi::looks_rtl(&span.text)
                        {
                            // Backtracking span with a real baseline offset,
                            // under the soft-wrap thresholds above: displayed
                            // math draws a fraction's denominator AFTER the
                            // relation sign that follows the numerator, so
                            // the next span starts at-or-left-of the previous
                            // span's ORIGIN with an overlap far beyond
                            // kerning (a denominator sits ~2 em back at a
                            // ~0.3 em baseline offset; stacked column cells
                            // land at delta_x ≈ 0 a row-pitch down). Bare
                            // concatenation fused these into tokens like
                            // "=dt" — break the line instead. Same-baseline
                            // kerned runs (y_diff ≈ 0) and forward-advancing
                            // subscripts (gap > -1 em) never reach here.
                            if !hangul_midword_wrap && !text.ends_with('\n') {
                                text.push('\n');
                            }
                        } else if prev.font_name != span.font_name
                            && span_end_x > prev_end_x + 0.5
                            && !text.ends_with(' ')
                            && !text.ends_with('\n')
                        {
                            text.push(' ');
                        } else if delta_x > fs * 1.5
                            && !text.ends_with(' ')
                            && !text.ends_with('\n')
                            && !Self::is_reliable_kerning_overlap(prev, span, gap)
                        {
                            // Inflated-width overlap recovery.
                            // A negative raw gap here usually comes from a
                            // font whose `/Widths` array is missing
                            // `FontInfo::new` fell back to the 550/1000-em
                            // constant, which over-reports each glyph's
                            // advance and drags `prev_end_x` past the real
                            // start of the next span. When the two spans'
                            // actual origins (`delta_x`) are separated by
                            // more than 1.5 em, they cannot both belong to
                            // the same word — the overlap is a width-table
                            // artifact, not real kerning — so insert a
                            // space to preserve the word boundary. This
                            // rescues cases like "STATION" + "FREEDOM"
                            // "UTILIZATION" + "CONFERENCE" in the NASA
                            // Apollo report header where raw gaps of
                            // -1.75 pt and -12.75 pt sit alongside
                            // delta_x values of 56 pt and 78 pt.
                            text.push(' ');
                        }
                    } else if y_diff > 2.0
                        && gap > FORWARD_GAP_K * prev.font_size.max(span.font_size).max(1.0)
                    {
                        // Forward-gap guard: pairs newly admitted to same-line
                        // handling by the widened threshold get a column/field-
                        // boundary check against FORWARD_GAP_K * max(fs).
                        // the constant's doc comment for calibration notes.
                        if !text.ends_with('\n') {
                            text.push('\n');
                        }
                    } else if prev.font_name != span.font_name
                        && gap > 0.5
                        && gap < prev.font_size.max(span.font_size).max(6.0) * 3.0
                        && !text.ends_with(' ')
                        && !text.ends_with('\n')
                    {
                        // Same-line font transition with a meaningful
                        // positive gap. Cross-font runs that survive the
                        // upstream `cross_font_word_glue` merge (i.e.
                        // both sides are multi-char) are word boundaries
                        // even when the gap is too small for the generic
                        // `should_insert_space` threshold (0.15 × fs) —
                        // e.g. roman → italic transitions in academic
                        // paper headers sit at ~2.7 pt at 10.9 pt body.
                        text.push(' ');
                    } else if Self::should_insert_space(prev, span) {
                        text.push(' ');
                    } else {
                        let fs = span.font_size.max(prev.font_size).max(6.0);
                        if gap > fs * 3.0 {
                            text.push('\n');
                        }
                    }
                }

                Self::push_span_text(&mut text, span);
                prev_span = Some(span.clone());
            }

            // Drain any tables that sit below all flow spans (or the
            // page had no flow spans at all). Without this final
            // pass they would be silently dropped now that the
            // end-of-page `for table in tables` block has been
            // removed.
            while let Some((_, table)) = pending_tables.pop() {
                flush_table(&mut text, table);
            }
            text
        };

        // Annotation text is already included via annotation_content_spans() in
        // extract_spans() — do NOT call append_non_widget_annotation_text() here,
        // as that would emit every annotation a second time.

        // Filter leaked PDF metadata
        let final_text = Self::filter_leaked_metadata(&text);

        // Normalize Kangxi Radicals
        let final_text = Self::normalize_kangxi_radicals(&final_text);

        // Normalize Arabic Presentation Forms
        let final_text = Self::normalize_arabic_presentation_forms(&final_text);

        let cleaned_text = final_text;

        // For tagged PDFs, the structure-tree traversal at line 4306 already
        // captures all table-cell content via MCIDs. Appending tables here
        // would double-emit that content (structure-tree text + table render),
        // dropping precision. For untagged PDFs, tables are inlined via
        // pending_tables above, so this block is never reached (cached_tree
        // is None → condition would be false). The block is removed.

        // #317 UTF-8 mojibake repair: a run of Latin-1 Supplement chars
        // whose raw bytes form valid UTF-8 decoding to non-Latin-1 code
        // points is almost certainly a double-encoded non-Latin string
        // (Cyrillic, Greek, CJK, Arabic, Hebrew, …) that surfaced
        // because the producing font had no ToUnicode CMap and the
        // /Differences / AGL lookup returned the UTF-8 byte sequence
        // re-interpreted as Latin-1. Re-decode those runs in place.
        let cleaned_text = Self::repair_utf8_mojibake(&cleaned_text);

        // Optionally expand Latin ligature characters to their component letters.
        let cleaned_text = if options.expand_ligatures {
            cleaned_text
                .replace('\u{FB00}', "ff")
                .replace('\u{FB01}', "fi")
                .replace('\u{FB02}', "fl")
                .replace('\u{FB03}', "ffi")
                .replace('\u{FB04}', "ffl")
                .replace(['\u{FB05}', '\u{FB06}'], "st")
        } else {
            cleaned_text
        };

        // Drop stray spaces a producer inserted between a CJK ideograph and an
        // embedded ASCII number (e.g. "公元前 1000 年" → "公元前1000年").
        let cleaned_text = crate::extractors::text::strip_cjk_digit_boundary_spaces(&cleaned_text);

        // Drop a space the word-break heuristic injected inside a prime-notation
        // number (e.g. "0′′ .28" / "0′′. 28" → "0′′.28").
        let cleaned_text =
            crate::extractors::text::strip_prime_decimal_boundary_spaces(&cleaned_text);

        Ok(cleaned_text)
    }
}
