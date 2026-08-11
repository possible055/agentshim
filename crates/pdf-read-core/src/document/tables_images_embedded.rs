use super::parsing::*;
use super::preflight::*;
use super::*;

impl PdfDocument {
    /// Extract tables from a page using structure tree and spatial detection.
    ///
    /// Tries two strategies in order:
    /// 1. **Structure tree** (tagged PDFs): Finds Table elements in the structure
    ///    tree and extracts cell content via MCID matching.
    /// 2. **Spatial detection** (untagged PDFs): Uses X/Y coordinate clustering
    ///    to detect grid-aligned text as tables.
    ///
    /// Returns early with structure tree tables if found (high confidence).
    pub(super) fn extract_page_tables(
        &self,
        page_index: usize,
        spans: &[TextSpan],
        options: &crate::converters::ConversionOptions,
        text_fallback: bool,
    ) -> Vec<crate::structure::Table> {
        // Strategy 1: Structure tree (tagged PDFs)
        let struct_tree_opt = {
            let cached = self.structure_tree_cache.lock_or_recover().clone();
            match cached {
                Some(tree) => tree,
                None => {
                    let is_marked = self.mark_info().map(|m| m.marked).unwrap_or(false);
                    let has_struct_tree_root = !is_marked
                        && self
                            .catalog()
                            .ok()
                            .and_then(|cat| cat.as_dict().map(|d| d.contains_key("StructTreeRoot")))
                            .unwrap_or(false);
                    let tree = if is_marked || has_struct_tree_root {
                        self.structure_tree().ok().flatten().map(Arc::new)
                    } else {
                        None
                    };
                    *self.structure_tree_cache.lock_or_recover() = Some(tree.clone());
                    tree
                }
            }
        };
        if let Some(ref struct_tree) = struct_tree_opt {
            // Build the per-page Table-element buckets once, then look up.
            if self.table_elements_cache.lock_or_recover().is_none() {
                let all = crate::structure::find_table_elements_all_pages(struct_tree);
                *self.table_elements_cache.lock_or_recover() = Some(all);
            }
            let table_elems: Vec<crate::structure::StructElem> = self
                .table_elements_cache
                .lock_or_recover()
                .as_ref()
                .and_then(|c| c.get(&(page_index as u32)))
                .cloned()
                .unwrap_or_default();
            if !table_elems.is_empty() {
                let mut tables = Vec::new();
                for table_elem in &table_elems {
                    match crate::structure::extract_table_from_spans(table_elem, spans) {
                        Ok(mut table) if !table.is_empty() => {
                            // Compute bbox from spans matching the table's MCIDs
                            if table.bbox.is_none() {
                                let all_mcids: HashSet<u32> = table
                                    .rows
                                    .iter()
                                    .flat_map(|r| {
                                        r.cells.iter().flat_map(|c| c.mcids.iter().copied())
                                    })
                                    .collect();
                                if !all_mcids.is_empty() {
                                    let mut min_x = f32::INFINITY;
                                    let mut min_y = f32::INFINITY;
                                    let mut max_x = f32::NEG_INFINITY;
                                    let mut max_y = f32::NEG_INFINITY;
                                    for span in spans {
                                        if let Some(mcid) = span.mcid {
                                            if all_mcids.contains(&mcid) {
                                                min_x = min_x.min(span.bbox.x);
                                                min_y = min_y.min(span.bbox.y);
                                                max_x = max_x.max(span.bbox.x + span.bbox.width);
                                                max_y = max_y.max(span.bbox.y + span.bbox.height);
                                            }
                                        }
                                    }
                                    if min_x < max_x && min_y < max_y {
                                        table.bbox = Some(crate::geometry::Rect::new(
                                            min_x,
                                            min_y,
                                            max_x - min_x,
                                            max_y - min_y,
                                        ));
                                    }
                                }
                            }
                            tables.push(table);
                        }
                        _ => {}
                    }
                }
                if !tables.is_empty() {
                    log::debug!(
                        "Found {} table(s) via structure tree for page {}",
                        tables.len(),
                        page_index
                    );
                    return tables;
                }
            }
        }

        // Strategy 2: Hybrid spatial detection (v0.3.14)
        let mut config = options.table_detection_config.clone().unwrap_or_default();
        // Honour the caller's text_fallback choice regardless of the default
        // on `TableDetectionConfig` — `extract_text` / `to_plain_text` pass
        // `text_fallback=false` to opt out of text-only spatial fallback even
        // though the type-level default is `true`.
        config.text_fallback = text_fallback;

        // Extract vector paths (lines/rects) for visual detection
        let paths = self.extract_paths(page_index).unwrap_or_default();

        // Filter to table-relevant paths (lines and rectangles only).
        // Chart/plot pages often have hundreds of curves and fills that
        // extract_edges ignores anyway — passing them through the full
        // detection pipeline wastes O(n²) time.
        const LINE_TOL: f32 = 2.0;
        let table_paths: Vec<_> = paths
            .into_iter()
            .filter(|p| {
                p.is_horizontal_line(LINE_TOL) || p.is_vertical_line(LINE_TOL) || p.is_rectangle()
            })
            .collect();

        // A page with thousands of line/rect paths is a drawing or chart, not a
        // ruled table; skip the O(E²) collinear-join + intersection sweep. Real
        // ruled tables have at most a few hundred edges. (Tagged tables already
        // returned above via the structure tree.)
        const MAX_TABLE_EDGES: usize = 1500;
        if table_paths.len() > MAX_TABLE_EDGES {
            log::debug!(
                "Page {} has {} line/rect paths (> {}) — skipping spatial table sweep",
                page_index,
                table_paths.len(),
                MAX_TABLE_EDGES
            );
            return Vec::new();
        }

        if table_paths.is_empty() {
            use crate::structure::spatial_table_detector::TableStrategy;
            let is_text_only = matches!(
                (config.horizontal_strategy, config.vertical_strategy),
                (TableStrategy::Text, TableStrategy::Text)
            );
            if !is_text_only && !config.text_fallback {
                return Vec::new();
            }
            if !is_text_only && config.text_fallback {
                log::debug!(
                    "No ruling lines on page {} — using text-only spatial fallback (issue #486)",
                    page_index
                );
            }
        }
        let paths = table_paths;

        let words = self.extract_words(page_index).unwrap_or_default();
        let word_spans: Vec<crate::layout::TextSpan> = words
            .into_iter()
            .map(|w| crate::layout::TextSpan {
                provenance: None,
                artifact_type: None,
                text: w.text,
                bbox: w.bbox,
                font_name: w.dominant_font,
                font_size: w.avg_font_size,
                font_weight: if w.is_bold {
                    crate::layout::FontWeight::Bold
                } else {
                    crate::layout::FontWeight::Normal
                },
                is_italic: w.is_italic,
                is_monospace: false,
                color: crate::layout::Color::black(),
                mcid: w.mcid,
                mcid_scope: None,
                sequence: 0,
                split_boundary_before: false,
                offset_semantic: false,
                char_spacing: 0.0,
                word_spacing: 0.0,
                horizontal_scaling: 1.0,
                primary_detected: false,
                char_widths: vec![],
                char_x_offsets: Vec::new(),
                heading_level: None,
                rotation_degrees: 0.0,
                wmode: 0,
                text_rise: 0.0,
                rtl_draw_logical: false,
            })
            .collect();

        // Fall back to raw spans if word extraction failed
        let input_spans = if !word_spans.is_empty() {
            &word_spans
        } else {
            spans
        };

        let raw_tables = crate::structure::spatial_table_detector::detect_tables_with_lines(
            input_spans,
            &paths,
            &config,
        );

        // Issue 484/486/487: when a logical multi-row table is drawn with a
        // horizontal ruling line between every pair of rows, the line-based
        // detector emits one Table per row strip. Each fragment is a 1- or
        // 2-row table that fails is_real_grid below and gets dropped, after
        // which the cells fall through to paragraph flow with column-based
        // reading order — producing orphan `<p>40000≤Q</p>` /
        // `<p>＜55000</p>` pairs. Consolidate vertically-adjacent fragments
        // that share an identical column structure BEFORE applying
        // is_real_grid so the merged multi-row table survives the filter.
        let raw_tables =
            crate::structure::spatial_table_detector::consolidate_adjacent_table_fragments(
                raw_tables,
            );

        // Step 4: spatial detection without struct-tree backing
        // is prone to false positives on form-style layouts (label-colon-
        // value pairs that align horizontally, form fillable boxes drawn
        // with thin lines). Drop tables that don't look like real grids.
        let raw_count = raw_tables.len();
        let mut tables: Vec<crate::structure::Table> = raw_tables
            .into_iter()
            .filter(|t| t.is_real_grid())
            // Prose-shape filter — applies to line-based detection too: a
            // PDF with decorative horizontal rules (newsletter mastheads,
            // press-release banners) can hand `is_real_grid` a "wide data
            // table" that is actually wrapped paragraphs partitioned by
            // word x-alignment. Reject those before they reach the
            // converter. See `looks_like_prose_table` for the heuristic.
            .filter(|t| !looks_like_prose_table(t))
            .collect();

        if raw_count != tables.len() {
            log::debug!(
                "Spatial table detection: filtered {} non-real-grid candidates on page {} ({} kept)",
                raw_count - tables.len(),
                page_index,
                tables.len(),
            );
        } else if !tables.is_empty() {
            log::debug!(
                "Found {} table(s) via hybrid spatial detection for page {}",
                tables.len(),
                page_index
            );
        }

        // Text-only spatial fallback for converter paths (to_markdown / to_html — issue #486).
        //
        // Wide data tables (e.g. sailing-score grids with 16-18 columns) exceed the default
        // `max_table_columns: 15` limit and are rejected by the main pipeline. When the
        // caller explicitly opted in to text-only detection (text_fallback=true), retry with
        // a relaxed config that raises the column ceiling and adjusts tolerances so that
        // genuinely wide data tables are captured.
        //
        // Safety guards:
        // - Only fires when the main pipeline returned no tables (avoids double-counting).
        // - Only fires when the caller is a converter (text_fallback=true).
        // - Skipped for tagged PDFs: the structure tree already provides the authoritative
        //   layout; spatial heuristics produce false-positive tables from structure elements
        //   (e.g. headings detected as single-row tables — issue #486 regression).
        // - Skipped for predominantly-RTL pages: Arabic/Hebrew text alignment patterns
        //   mimic table columns in spatial heuristics — issue #486 regression.
        // - When ruling lines exist, spans are filtered to the line-bounded region to
        //   prevent page headers/footers from being erroneously included in the table.
        // - Results must pass is_real_grid() just like main-pipeline tables.

        // Guard 1 — Tagged PDFs: presence of a structure tree means the document has an
        // explicit semantic layout. Spatial text-only detection would misfire on
        // structure elements (headings, paragraphs) that happen to share a Y band.
        if config.text_fallback && struct_tree_opt.is_some() {
            log::debug!(
                "Text-only spatial fallback skipped for page {} — document has a structure tree (tagged PDF)",
                page_index
            );
            return tables;
        }

        // Guard 2 — RTL pages: Arabic and Hebrew text naturally aligns horizontally in
        // patterns that the column-clustering algorithm mistakes for table columns.
        // Skip spatial detection when more than 30 % of the input spans are RTL.
        if config.text_fallback {
            let rtl_count = input_spans
                .iter()
                .filter(|s| crate::text::bidi::looks_rtl(&s.text))
                .count();
            let rtl_fraction = rtl_count as f32 / input_spans.len().max(1) as f32;
            if rtl_fraction > 0.30 {
                log::debug!(
                    "Text-only spatial fallback skipped for page {} — {:.0}% RTL spans (threshold 30%)",
                    page_index,
                    rtl_fraction * 100.0
                );
                return tables;
            }
        }

        if config.text_fallback && tables.is_empty() {
            use crate::structure::spatial_table_detector::detect_tables_from_spans_column_aware;
            // Build a relaxed config derived from the caller's config.
            // We only raise the limits known to block wide data tables (e.g. sailing
            // score grids with 16-18 columns that exceed the default max_table_columns=15).
            let relaxed_config = crate::structure::spatial_table_detector::TableDetectionConfig {
                // Allow up to 25 columns — covers 17-column sailing score tables.
                max_table_columns: config.max_table_columns.max(25),
                // Tighter column grouping than the default 15 pt so that nearby
                // score columns are not merged into each other.
                column_tolerance: config.column_tolerance.min(10.0),
                // Looser merge threshold so that columns with slight X scatter
                // (e.g. centred numeric cells) are aggregated correctly.
                column_merge_threshold: config.column_merge_threshold.max(30.0),
                // Inherit all other settings from caller's config.
                ..config.clone()
            };

            // When ruling lines are present on the page, restrict text detection to
            // spans that fall within the VERTICAL-LINE Y bounds. Vertical lines
            // define the table's column structure and their Y extent precisely
            // delineates the table rows, excluding page headers and footers which
            // sit above/below the table frame.
            //
            // Note: we use V-line Y bounds specifically (not total path bbox) because
            // H-lines in these PDFs often span the full page height (outer frame),
            // while V-lines are confined to the interior table region.
            let candidate_spans: Vec<crate::layout::TextSpan>;
            let fallback_spans: &[crate::layout::TextSpan] = {
                let v_lines: Vec<_> = paths.iter().filter(|p| p.is_vertical_line(2.0)).collect();
                if !v_lines.is_empty() {
                    // Rendered extents: a stroke-width-encoded column rule's
                    // drawn bar spans the table height while its geometric
                    // bbox is a ~0pt speck at the midline —
                    // banding on the speck would filter out the table's own
                    // spans.
                    let vline_y_min = v_lines
                        .iter()
                        .map(|p| p.rendered_bbox().y)
                        .fold(f32::INFINITY, f32::min);
                    let vline_y_max = v_lines
                        .iter()
                        .map(|p| {
                            let r = p.rendered_bbox();
                            r.y + r.height
                        })
                        .fold(f32::NEG_INFINITY, f32::max);
                    // Small margin to include spans whose centres just touch the frame.
                    const V_MARGIN: f32 = 5.0;
                    candidate_spans = input_spans
                        .iter()
                        .filter(|s| {
                            let cy = s.bbox.y + s.bbox.height * 0.5;
                            cy >= vline_y_min - V_MARGIN && cy <= vline_y_max + V_MARGIN
                        })
                        .cloned()
                        .collect();
                    log::debug!(
                        "Text fallback (page {}): V-lines Y=[{:.1},{:.1}] — filtered {} spans to {}",
                        page_index,
                        vline_y_min,
                        vline_y_max,
                        input_spans.len(),
                        candidate_spans.len()
                    );
                    &candidate_spans
                } else {
                    input_spans
                }
            };

            let text_candidates =
                detect_tables_from_spans_column_aware(fallback_spans, &relaxed_config);
            let pre_filter = text_candidates.len();
            let text_tables: Vec<_> = text_candidates
                .into_iter()
                // Text-only detection infers columns from word x-alignment
                // alone; a title + a wrapped body line (two rows) is the
                // signature of ordinary prose, not a table. Require ≥3
                // rows of evidence before promoting to a table.
                .filter(|t| t.rows.len() >= 3 && t.is_real_grid())
                // Prose split across many "columns" is the dominant
                // false-positive shape for text-only detection on
                // line-less pages: a paragraph wraps to N lines, words
                // cluster into N×K cells, and `is_real_grid` accepts the
                // shape. Real data-table cells almost never end with a
                // comma or semicolon (those punctuation marks belong to
                // running sentences), so a high comma-tail ratio is the
                // most discriminating prose signal we have.
                .filter(|t| !looks_like_prose_table(t))
                .collect();
            if !text_tables.is_empty() {
                log::debug!(
                    "Text-only relaxed fallback found {} table(s) on page {} ({} filtered by is_real_grid) — issue #486",
                    text_tables.len(),
                    page_index,
                    pre_filter - text_tables.len(),
                );
                tables = text_tables;
            }
        }

        tables
    }

    /// Extract images from a page.
    ///
    /// Extracts all images from the specified page by processing the content stream.
    /// This includes:
    /// - Images referenced via `Do` operators (XObject calls)
    /// - Images in nested Form XObjects (with recursion)
    /// - Inline images (BI...ID...EI sequences)
    ///
    /// This method processes PDF content streams instead of only iterating the XObject
    /// dictionary. This ensures that images referenced via the `Do` operator in the content
    /// stream are properly extracted, including those in nested Form XObjects. ColorSpace
    /// indirect references are also resolved.
    ///
    /// Returns a vector of PdfImage objects representing the extracted images.
    ///
    /// # Arguments
    ///
    /// * `page_index` - Zero-based page index
    ///
    /// # Returns
    ///
    /// A vector of PdfImage objects, one for each image found on the page.
    ///
    /// # Errors
    ///
    /// Returns an error if the page cannot be accessed or if image extraction fails.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use pdf_oxide::document::PdfDocument;
    /// # let mut doc = PdfDocument::open("sample.pdf")?;
    /// let images = doc.extract_images(0)?;
    /// println!("Found {} images on page 1", images.len());
    /// for (i, image) in images.iter().enumerate() {
    ///     image.save_as_png(&format!("image_{}.png", i))?;
    /// }
    /// # Ok::<(), pdf_oxide::error::Error>(())
    /// ```
    pub fn extract_images(&self, page_index: usize) -> Result<Vec<crate::extractors::PdfImage>> {
        self.require_authenticated()?;
        self.extract_images_filtered(page_index, &ImageExtractFilter::default())
    }

    /// WS2.6: normalized top/bottom-band text lines that recur on a majority
    /// of pages — running headers/footers. Page-number digits are stripped so
    /// "Page 3"/"Page 4" collapse to one signature. Empty for documents under
    /// 3 pages (repetition can't be judged) or when nothing repeats.
    pub(crate) fn repeated_running_head_foot(
        &self,
        threshold: f32,
    ) -> std::collections::HashSet<String> {
        use std::collections::{HashMap, HashSet};
        let mut out: HashSet<String> = HashSet::new();
        let Ok(page_count) = self.page_count() else {
            return out;
        };
        if page_count < 3 {
            return out;
        }
        let min_occ = (((page_count as f32) * threshold).ceil() as usize).max(2);
        let mut occ: HashMap<String, usize> = HashMap::new();
        for p in 0..page_count {
            let media_h = self.get_page_media_box(p).map(|m| m.3).unwrap_or(792.0);
            let Ok(spans) = self.extract_spans(p) else {
                continue;
            };
            let mut seen: HashSet<String> = HashSet::new();
            for s in &spans {
                if !Self::in_head_foot_band(s, media_h) {
                    continue;
                }
                let norm = Self::normalize_band_line(&s.text);
                // Count each distinct line once per page.
                if norm.len() > 3 && seen.insert(norm.clone()) {
                    *occ.entry(norm).or_default() += 1;
                }
            }
        }
        for (text, n) in occ {
            if n >= min_occ {
                out.insert(text);
            }
        }
        out
    }

    /// True when a span sits in the top or bottom 15% band of the page.
    pub(super) fn in_head_foot_band(s: &crate::layout::TextSpan, media_h: f32) -> bool {
        s.bbox.y > media_h * 0.85 || (s.bbox.y + s.bbox.height) < media_h * 0.15
    }

    /// Normalize a band line for repetition matching: drop ASCII digits (page
    /// numbers vary), collapse whitespace, lowercase.
    pub(super) fn normalize_band_line(text: &str) -> String {
        let stripped: String = text.chars().filter(|c| !c.is_ascii_digit()).collect();
        stripped
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    }

    /// True when `span` is a running header/footer to strip: in the top/bottom
    /// band and its normalized text is one of the `repeated` signatures.
    pub(super) fn is_running_head_foot(
        span: &crate::layout::TextSpan,
        media_h: f32,
        repeated: &std::collections::HashSet<String>,
    ) -> bool {
        Self::in_head_foot_band(span, media_h)
            && repeated.contains(&Self::normalize_band_line(&span.text))
    }

    /// Extract embedded files / attachments (WS1.8a, ISO 32000-1 §7.11.4).
    ///
    /// Walks the catalog's `/Names /EmbeddedFiles` name tree and returns
    /// `(filename, decoded bytes)` for each `/Filespec`'s `/EF /F` (or `/UF`)
    /// embedded-file stream. Complements the existing embedded-file writer.
    /// Returns an empty vector when the document has no attachments.
    pub fn extract_embedded_files(&self) -> Result<Vec<(String, Vec<u8>)>> {
        self.require_authenticated()?;
        let catalog = self.catalog()?;
        let Some(cat_dict) = catalog.as_dict() else {
            return Ok(Vec::new());
        };
        // catalog → /Names → /EmbeddedFiles (name-tree root).
        let names = cat_dict
            .get("Names")
            .and_then(|n| self.resolve_object(n).ok());
        let Some(ef_root) = names
            .as_ref()
            .and_then(|n| n.as_dict())
            .and_then(|d| d.get("EmbeddedFiles"))
            .and_then(|e| self.resolve_object(e).ok())
        else {
            return Ok(Vec::new());
        };

        let mut filespecs: Vec<Object> = Vec::new();
        self.collect_embedded_filespecs(&ef_root, &mut filespecs, 0);

        let mut out = Vec::new();
        for fs in &filespecs {
            let Some(fs_dict) = fs.as_dict() else {
                continue;
            };
            // Prefer the Unicode filename /UF, fall back to /F.
            let filename = fs_dict
                .get("UF")
                .or_else(|| fs_dict.get("F"))
                .and_then(|n| self.resolve_object(n).ok())
                .and_then(|n| {
                    n.as_string()
                        .map(|s| String::from_utf8_lossy(s).into_owned())
                })
                .unwrap_or_else(|| "attachment".to_string());
            // /EF → { /F <stream ref> } (or /UF).
            let Some(ef) = fs_dict.get("EF").and_then(|e| self.resolve_object(e).ok()) else {
                continue;
            };
            let Some(ef_dict) = ef.as_dict() else {
                continue;
            };
            let Some(stream_ref) = ef_dict
                .get("F")
                .or_else(|| ef_dict.get("UF"))
                .and_then(|r| r.as_reference())
            else {
                continue;
            };
            if let Ok(stream_obj) = self.load_object(stream_ref) {
                if let Ok(bytes) = self.decode_stream_with_encryption(&stream_obj, stream_ref) {
                    out.push((filename, bytes));
                }
            }
        }
        Ok(out)
    }

    /// Recursively collect `/Filespec` objects from an `/EmbeddedFiles`
    /// name-tree node: leaf `/Names [key filespec …]` pairs plus `/Kids`.
    pub(super) fn collect_embedded_filespecs(
        &self,
        node: &Object,
        out: &mut Vec<Object>,
        depth: u8,
    ) {
        if depth > 32 {
            return;
        }
        let Ok(node) = self.resolve_object(node) else {
            return;
        };
        let Some(dict) = node.as_dict() else {
            return;
        };
        if let Some(names) = dict.get("Names").and_then(|n| self.resolve_object(n).ok()) {
            if let Some(arr) = names.as_array() {
                // Flat [key1 filespec1 key2 filespec2 …]: the odd indices.
                let mut i = 1;
                while i < arr.len() {
                    if let Ok(fs) = self.resolve_object(&arr[i]) {
                        out.push(fs);
                    }
                    i += 2;
                }
            }
        }
        if let Some(kids) = dict.get("Kids").and_then(|k| self.resolve_object(k).ok()) {
            if let Some(arr) = kids.as_array() {
                for kid in arr {
                    self.collect_embedded_filespecs(kid, out, depth + 1);
                }
            }
        }
    }

    /// Build the resource-name → colour-space-object map from a resolved
    /// `/Resources` dictionary's `/ColorSpace` subdictionary (§8.6.3 / §7.8.3),
    /// resolving one indirect-ref hop per entry so the stored value is a colour
    /// space name or array. Empty when there is no `/ColorSpace` subdictionary;
    /// the standard device names parse directly and need no entry. Consumed by
    /// the image-handle builders so `decode()` / the handle's `color_space` can
    /// resolve names like `/CS0` (§8.6.6, §8.9.7).
    pub(super) fn build_color_space_map(
        &self,
        resources: Option<&Object>,
    ) -> std::collections::HashMap<String, Object> {
        let mut map = std::collections::HashMap::new();
        let Some(res) = resources else {
            return map;
        };
        let res = if let Some(r) = res.as_reference() {
            match self.load_object(r) {
                Ok(o) => o,
                Err(_) => return map,
            }
        } else {
            res.clone()
        };
        let Some(res_dict) = res.as_dict() else {
            return map;
        };
        let Some(cs_entry) = res_dict.get("ColorSpace") else {
            return map;
        };
        let cs_obj = if let Some(r) = cs_entry.as_reference() {
            match self.load_object(r) {
                Ok(o) => o,
                Err(_) => return map,
            }
        } else {
            cs_entry.clone()
        };
        let Some(cs_dict) = cs_obj.as_dict() else {
            return map;
        };
        for (name, value) in cs_dict.iter() {
            let resolved = if let Some(r) = value.as_reference() {
                self.load_object(r).unwrap_or_else(|_| value.clone())
            } else {
                value.clone()
            };
            map.insert(name.clone(), resolved);
        }
        map
    }
}
