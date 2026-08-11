use super::*;

pub(super) fn block_margins(
    sorted: &[&OrderedTextSpan],
) -> (
    std::collections::HashMap<u32, f32>,
    std::collections::HashMap<u32, f32>,
) {
    let mut block_right_max: std::collections::HashMap<u32, f32> = std::collections::HashMap::new();
    let mut block_left_min: std::collections::HashMap<u32, f32> = std::collections::HashMap::new();
    for s in sorted {
        if let Some(b) = s.block_id {
            let right = s.span.bbox.x + s.span.bbox.width;
            let e = block_right_max.entry(b).or_insert(f32::MIN);
            if right > *e {
                *e = right;
            }
            let l = block_left_min.entry(b).or_insert(f32::MAX);
            if s.span.bbox.x < *l {
                *l = s.span.bbox.x;
            }
        }
    }
    (block_right_max, block_left_min)
}

pub(super) fn finalize_markdown(
    result: String,
    sorted: &[&OrderedTextSpan],
    config: &TextPipelineConfig,
    footnote_plan: &FootnotePlan,
) -> String {
    // Final whitespace normalization
    let mut final_result = if config.output.preserve_layout {
        result
    } else {
        let cleaned = result
            .split("\n\n")
            .map(|para| para.trim())
            .filter(|para| !para.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");

        if result.ends_with('\n') && !cleaned.ends_with('\n') {
            format!("{}\n", cleaned)
        } else {
            cleaned
        }
    };

    // Merge key-value pairs that were split across lines due to column-based
    // reading order (e.g. "Grand Total\n$750.00" → "Grand Total $750.00").
    final_result = super::merge_key_value_pairs(&final_result);

    // Band-aid post-processing for known extraction-quality issues
    // reported against v0.3.51/v0.3.52 markdown output. The deeper
    // fixes (root-cause changes to the spatial-table detector,
    // heading-fragmentation prevention upstream, font-CMap recovery)
    // happen on follow-up branches; these post-process steps remove
    // the most damaging surface symptoms so downstream consumers
    // (LLM ingestion, RAG pipelines) get usable text now.
    //
    // Step order is deliberate:
    //   1. Pipe escape — clean up stray pipes BEFORE table-block
    //      detection runs again in subsequent steps.
    //   2. Degenerate-table simplification (#3, #6, partial #11).
    //   3. Heading merge (#1, #4) — only after degenerate tables
    //      have been collapsed so leftover heading fragments are
    //      contiguous and visible to the merger.
    //   4. Page-number filter (#9).
    //   5. Bullet glyph normalization (#13).
    //
    // SPEC-ALIGNMENT GATE (ISO 32000-1:2008 §14.8.4). When the
    // document carries an explicit structure tree — any span has a
    // resolved `struct_role` — the heading levels, table cells, and
    // block boundaries are AUTHORITATIVE per the spec
    // (§14.8.4.3.2: each H/H1-H6 is a distinct heading element).
    // In that case we must NOT apply the layout-recovery heuristics
    // that guess at structure, because they could override correct,
    // author-specified tagging (e.g. fuse three legitimately-
    // distinct H1 sections). The heuristic structure recovery is
    // ONLY valid for UNTAGGED documents, where the markdown
    // structure was itself derived heuristically (font-size ratios,
    // spatial grouping) and is therefore fair game to refine.
    let is_tagged = sorted.iter().any(|s| s.struct_role.is_some());

    // Always-safe steps (no semantic structure change): markdown
    // escaping, whitespace-only bold-fragment recovery, and
    // exact-duplicate paragraph dedup. These run for both tagged
    // and untagged documents.
    final_result = escape_stray_leading_pipes(&final_result);
    final_result = coalesce_camelcase_bold_fragments(&final_result);
    // Fuse consecutive monospace (fixed-pitch / code-font) paragraphs into a
    // fenced code block. A `Code` element renders its lines monospace even
    // when the producer left the block untagged.
    final_result = fence_monospace_blocks(&final_result);
    // Tight lists: drop blank lines between consecutive list-item markers.
    // The span flush always appends a blank line, which turns every list
    // into a Markdown "loose" list (each item wrapped in <p>); the golden
    // corpus and most renderers expect a "tight" list. CommonMark §5.3.
    final_result = tighten_list_items(&final_result);

    // Structure-recovery heuristics — UNTAGGED documents only.
    // For tagged PDFs the structure tree is authoritative (§14.8.4)
    // so these are skipped.
    if !is_tagged {
        final_result = collapse_numeric_heading_runs(&final_result);
        final_result = merge_consecutive_same_level_headings(&final_result);
        // Line-level heading re-validation: the level is decided per span,
        // before the line's spans are concatenated, so a short promoted span
        // can accrete a body continuation into a long, sentence-like line.
        // Re-apply the heading gate to each COMPLETE line and demote body
        // text back to a paragraph (marker-style block-level classification).
        // Tagged PDFs are skipped — their `/H` tags are authoritative.
        final_result = MarkdownOutputConverter::demote_body_like_headings(&final_result);
    }
    // INTENTIONALLY NOT INVOKED — these would damage legitimate
    // content and were removed after a 70-PDF baseline-vs-HEAD
    // regression sweep proved real-world breakage:
    //
    //  * simplify_degenerate_tables — flattened a REAL country-
    //    data table (google_doc_document.pdf: countries × Continent
    //    / Capital / Currency / Population) into one prose line,
    //    because legitimate tables can be mostly single-word. A
    //    markdown-layer heuristic cannot reliably tell a spurious
    //    multi-column-prose "table" from a real sparse one. The
    //    correct fix is upstream: stop the spatial-table detector
    //    from firing on prose columns in the first place.
    //  * dedup_consecutive_paragraphs — removed DISTINCT form
    //    widgets that share a label (annotation-button-widget.pdf:
    //    several real radio buttons all labelled "Radio button,
    //    unselected") and collapsed legitimately-repeated headings
    //    (ArabicCIDTrueType.pdf). "Looks duplicated" != "is an
    //    extraction artifact". The correct fix is upstream: stop
    //    the structured + plaintext paths from double-emitting.
    //  * filter_page_number_lines — dropped real "Page N" text;
    //    correct fix is `/Artifact` handling (§14.8.2.2).
    //  * normalize_bullet_glyphs — rewrote codepoints; correct fix
    //    is ToUnicode-CMap fallback (§9.10).
    //
    // dedup_identical_header_cells is also retired from the active
    // path: blanking "duplicate" header cells assumes the
    // duplication is an artifact, which the same content-
    // preservation principle rejects without upstream certainty.

    // Apply hyphenation reconstruction if enabled
    if config.enable_hyphenation_reconstruction {
        let handler = HyphenationHandler::new();
        final_result = handler.process_text(&final_result);
    }

    // RTL emphasis cleanup (#377 D7-fix). The original D7 also
    // unconditionally re-ordered each RTL line via
    // `reorder_visual_to_logical`, on the assumption that PDF
    // content streams always emit RTL runs in *visual* order. In
    // practice some PDFs (notably the pdfium hebrew_mirrored.pdf
    // test fixture and Arabic CID-TrueType samples) already store
    // text in *logical* order and our blanket reorder reversed
    // them again, breaking previously-correct output (`בנימין` →
    // `ןימינב`, `# heading` → `heading #`). Without a reliable
    // way to detect which order the source uses we drop the
    // reorder step. The other half of D7 — stripping spurious
    // `**bold**` / `*italic*` markers that the font-weight
    // detector emits around Arabic contextual glyph forms — is
    // safe and stays.
    if crate::text::bidi::looks_rtl(&final_result) {
        // `str::lines()` strips trailing newlines, so `join("\n")` would
        // silently drop a terminal `\n` (or `\n\n`) that the whitespace
        // normalisation step above carefully preserved.  Restore the
        // suffix after reassembly so callers see a consistent document.
        let trailing_newlines: String = final_result
            .chars()
            .rev()
            .take_while(|&c| c == '\n')
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        final_result = final_result
            .lines()
            .map(|line| {
                if crate::text::bidi::looks_rtl(line) {
                    strip_inline_emphasis_in_rtl(line)
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        if !trailing_newlines.is_empty() {
            final_result.push_str(&trailing_newlines);
        }
    }

    // Bidi-isolation markers (UAX #9 §2.4 — #537 follow-up).
    //
    // The v0.3.54 #537 detector landed the geometric visual-vs-
    // logical RTL classifier in `text::bidi::detect_visual_order_run`
    // and the extractor now reverses content-stream visual-order
    // runs into logical order. That fixed the *codepoint sequence*
    // (Hebrew letters appear in correct reading order).
    //
    // What it did not fix: bidi-rendering contamination at run
    // boundaries. When a markdown viewer (Pandoc, GitHub, VS Code
    // preview, Obsidian) reads a paragraph with mixed LTR + RTL
    // content and applies the Unicode Bidirectional Algorithm, the
    // *neutral* characters at run boundaries (parens, commas,
    // periods, spaces) migrate visually across the boundary
    // because they inherit direction from surrounding strong
    // characters. UAX #9 §2.4 fixes this with explicit isolation
    // markers: U+2067 RLI / U+2069 PDI around an RTL run inside an
    // LTR paragraph, U+2066 LRI / U+2069 PDI around an LTR run
    // inside an RTL paragraph.
    //
    // Markdown ONLY — `extract_text` and `PlainTextConverter` skip
    // this step. Plain-text consumers do not honour UAX #9 and
    // would render the markers as literal garbage. Per the v0.3.55
    // plan `docs/releases/plans/v0.3.55/fix-537-followup-bidi-isolation-markers.md`.
    if crate::text::bidi::looks_rtl(&final_result) {
        final_result = wrap_bidi_isolates_per_line(&final_result);
    }

    // Append confirmed footnote definitions as a trailing block of
    // `[^N]: text` lines, separated from the body by a blank line.
    // Emitted after all body post-processing so the definition
    // markers are never mistaken for table/list syntax. Empty when
    // no footnotes were confirmed (byte-identical output).
    if !footnote_plan.definitions.is_empty() {
        let trailing_nl = final_result
            .chars()
            .rev()
            .take_while(|&c| c == '\n')
            .count();
        for _ in trailing_nl..2 {
            final_result.push('\n');
        }
        for def in &footnote_plan.definitions {
            final_result.push_str(def);
            final_result.push('\n');
        }
    }

    final_result
}
