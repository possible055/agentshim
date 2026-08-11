use super::*;

impl MarkdownOutputConverter {
    /// Core rendering logic shared between convert() and convert_with_tables().
    pub(super) fn render_spans(
        &self,
        spans: &[OrderedTextSpan],
        tables: &[Table],
        config: &TextPipelineConfig,
    ) -> Result<String> {
        if spans.is_empty() && tables.is_empty() {
            return Ok(String::new());
        }

        // Sort by reading order
        let mut sorted: Vec<_> = spans.iter().collect();
        sorted.sort_by_key(|s| s.reading_order);

        // Body-font size for the heading-ratio reference. Span-count
        // mode bucketed to 0.5pt, with smaller-bucket tiebreak so body
        // text wins over headings when counts are close. Capped at 12pt
        // so that heading-only documents still produce sensible ratios.
        let base_font_size = super::base_heading_font_size(&sorted, config.output.detect_headings);

        // Footnote detection (WS2.7). Conservative, high-precision: an
        // inline superscript marker is only rewritten to `[^N]` when a
        // matching page-bottom definition block starting with the SAME
        // token is also present, and vice-versa. When either half is
        // missing the plan is empty and rendering is byte-identical to
        // before. See `detect_footnotes` for the heuristics.
        let footnote_plan = detect_footnotes(&sorted, base_font_size);

        let (block_right_max, block_left_min) = block_margins(&sorted);

        // Track which tables have been rendered
        let mut tables_rendered = vec![false; tables.len()];
        // Pre-render table markdown so we can check for orphaned spans.
        let table_mds: Vec<String> = tables
            .iter()
            .map(|t| self.render_table_markdown(t, config))
            .collect();
        // Collect spans skipped because they fall inside a table region.
        let mut table_skipped_spans: Vec<Vec<&OrderedTextSpan>> = vec![Vec::new(); tables.len()];

        let mut result = String::new();
        let mut prev_span: Option<&OrderedTextSpan> = None;
        let mut current_line = String::new();
        // Track open inline formatting to consolidate adjacent bold/italic spans.
        // When consecutive same-line spans share the same bold or italic style,
        // we keep the markers open and only close them when the style changes or
        // the line is flushed, producing e.g. **ACME GLOBAL LTD.** instead
        // of **ACME** **GLOBAL** **LTD.**.
        let mut active_bold = false;
        let mut active_italic = false;
        let mut current_heading_level: Option<u8> = None;
        // Whether every non-blank span accumulated into `current_line` so far is
        // monospace (a fixed-pitch / code font). A plain paragraph that is all
        // monospace is a code line; consecutive ones are fused into a fenced
        // block by `fence_monospace_blocks` after rendering.
        let mut current_line_all_mono = true;

        /// Close any open bold/italic markers on `line`.
        ///
        /// CommonMark forbids whitespace adjacent to closing emphasis markers
        /// (e.g. `**bold **` is rendered as literal asterisks). Strip trailing
        /// whitespace before closing, then restore it after the markers.
        fn close_formatting(line: &mut String, bold: &mut bool, italic: &mut bool) {
            if !*bold && !*italic {
                return;
            }
            let content_end = line.trim_end().len();
            let trailing_ws = line[content_end..].to_string();
            line.truncate(content_end);
            // Close in reverse order of opening: italic first, then bold.
            if *italic {
                line.push('*');
                *italic = false;
            }
            if *bold {
                line.push_str("**");
                *bold = false;
            }
            line.push_str(&trailing_ws);
        }

        // Strip markdown emphasis markers (**bold**, *italic*) from a line.
        // Used when emitting heading lines, where the `#` prefix already
        // provides emphasis and nested markers (e.g. `# **Title**`) are
        // redundant and can confuse strict CommonMark renderers.
        fn strip_emphasis(s: &str) -> String {
            let mut out = String::with_capacity(s.len());
            let chars: Vec<char> = s.chars().collect();
            let mut i = 0;
            while i < chars.len() {
                if chars[i] == '*' {
                    // Skip one or two asterisks
                    i += 1;
                    if i < chars.len() && chars[i] == '*' {
                        i += 1;
                    }
                    continue;
                }
                out.push(chars[i]);
                i += 1;
            }
            out
        }

        // WS2.5: left-edge levels for list-item nesting depth (inert on flat lists).
        let list_x_levels = Self::list_marker_x_levels(&sorted);

        for (idx, span) in sorted.iter().enumerate() {
            // Skip artifacts (pagination, headers, footers) unless the
            // caller opted in via `config.output.include_artifacts`
            // (default true).
            if !config.output.include_artifacts && span.span.artifact_type.is_some() {
                continue;
            }

            // Suppress spans that make up a confirmed page-bottom footnote
            // definition block: they are re-emitted as `[^N]: …` lines at
            // the end of the document instead of appearing inline as body
            // text. (Empty set when no footnotes were confirmed.)
            if footnote_plan.definition_spans.contains(&idx) {
                continue;
            }

            // Whether this span is a confirmed inline footnote marker
            // (a raised superscript token with a matching bottom def).
            let footnote_marker_label = footnote_plan.inline_markers.get(&idx);

            // Skip "noise" spans: isolated single-character fragments that
            // are purely punctuation/symbol (e.g. a bare "|" or "—" on its
            // own baseline from a decorative PDF separator). These add no
            // semantic value but pollute output as lone-line paragraphs.
            // Bullet characters are excluded from this filter since they
            // are meaningful list markers handled downstream.
            //
            // EXCEPTION: a space-bearing punctuation span (" ," / " .") that is
            // FLANKED by right-to-left text on its own baseline is a
            // producer-drawn inter-word separator, not decoration. RTL scripts
            // emit the comma/period between two words as its own span, so
            // dropping it glues the words ("הטורפים, ממשפחת" → "הטורפיםממשפחת").
            // The RTL-context gate is deliberate: a lone decorative mark, or an
            // isolated punctuation fragment in a left-to-right data region,
            // has no RTL neighbour on its line and is still dropped (keeping it
            // there would fragment the line into one paragraph per mark).
            {
                let t = span.span.text.trim();
                let char_count = t.chars().count();
                if char_count > 0
                    && char_count <= 2
                    && !t.chars().any(|c| c.is_alphanumeric())
                    && !Self::is_bullet_span(t)
                    && !Self::starts_with_bullet(t)
                    && footnote_marker_label.is_none()
                {
                    let carries_space = span.span.text.chars().any(|c| c.is_whitespace());
                    let rtl_separator = carries_space && {
                        let y = span.span.bbox.y;
                        let tol = span.span.font_size.max(1.0) * 1.5;
                        sorted.iter().enumerate().any(|(j, other)| {
                            j != idx
                                && (other.span.bbox.y - y).abs() <= tol
                                && crate::text::bidi::looks_rtl(&other.span.text)
                        })
                    };
                    if !rtl_separator {
                        continue;
                    }
                }
            }

            // Check if this span belongs to a table region
            if !tables.is_empty() {
                if let Some(table_idx) = super::span_in_table(span, tables) {
                    if !tables_rendered[table_idx] {
                        // Flush current line
                        close_formatting(&mut current_line, &mut active_bold, &mut active_italic);
                        if !current_line.is_empty() {
                            push_plain_paragraph(
                                &mut result,
                                current_line.trim(),
                                current_line_all_mono,
                            );
                            current_line.clear();
                        }

                        // Render the table
                        result.push_str(&table_mds[table_idx]);
                        result.push('\n');
                        tables_rendered[table_idx] = true;
                        prev_span = None;
                    }
                    // Track span for orphan recovery
                    table_skipped_spans[table_idx].push(span);
                    // Skip this span (it's part of a table)
                    continue;
                }
            }

            // Heading level: structure-tree role takes precedence over
            // font-size heuristics when the source PDF is tagged. This
            // is the issue #377 D1 unlock — Word/Acrobat tagged PDFs
            // that set body and heading text in the same point size
            // would otherwise lose all heading hierarchy.
            let span_heading_level = match span.struct_role {
                Some(StructRole::Heading(level)) => Some(level.clamp(1, 6)),
                _ if config.output.detect_headings => {
                    // Font-size heuristic first; fall back to numbered-section
                    // promotion (WS2.4) so same-point-size numbered headings
                    // ("2.1 Method") are still recovered.
                    self.heading_level_ratio(span, base_font_size).or_else(|| {
                        if Self::is_valid_heading_text(span.span.text.trim()) {
                            Self::numbered_heading_level(&span.span.text)
                        } else {
                            None
                        }
                    })
                }
                _ => None,
            };

            // List-item role from the structure tree. When set, we emit
            // a markdown `- ` bullet at the start of the line for this
            // span (mirroring `is_bullet_span`/`starts_with_bullet`
            // detection used for untagged docs).
            let is_list_item_role = matches!(
                span.struct_role,
                Some(StructRole::ListItemBody)
                    | Some(StructRole::ListItem)
                    | Some(StructRole::ListItemLabel)
            );

            // Whether this span's own text already carries an ordered marker
            // (`1.`, `a)`). Ordered items must not also get a `- ` prefix.
            let span_is_ordered =
                Self::is_ordered_list_marker(span.span.text.trim_start()).is_some();

            // Force-flush an open heading before a list item begins, so a bullet
            // or ordered marker never glues onto the heading line.
            let span_starts_list = is_list_item_role
                || Self::is_bullet_span(&span.span.text)
                || Self::starts_with_bullet(&span.span.text)
                || span_is_ordered;
            if span_starts_list && !current_line.trim().is_empty() {
                if let Some(level) = current_heading_level {
                    close_formatting(&mut current_line, &mut active_bold, &mut active_italic);
                    let prefix = "#".repeat(level as usize);
                    result.push_str(&format!(
                        "{} {}\n\n",
                        prefix,
                        strip_emphasis(current_line.trim())
                    ));
                    current_line.clear();
                    current_heading_level = None;
                }
            }

            // Check for paragraph break or line break
            let same_line = prev_span
                .map(|prev| (span.span.bbox.y - prev.span.bbox.y).abs() < span.span.font_size * 0.5)
                .unwrap_or(true);

            if let Some(prev) = prev_span {
                // Group boundary: when group_id changes, insert a paragraph break
                // to keep spatially partitioned regions (e.g. columns) contiguous.
                let group_changed = match (span.group_id, prev.group_id) {
                    (Some(a), Some(b)) => a != b,
                    _ => false,
                };

                let heading_changed = current_heading_level != span_heading_level;

                // A reading-order group change only forces a paragraph break
                // when the visual line also changes — this keeps horizontally
                // split elements (e.g. multi-span footer lines) together.
                let group_flush = group_changed && !same_line;

                let prev_was_list_item = matches!(
                    prev.struct_role,
                    Some(StructRole::ListItemBody)
                        | Some(StructRole::ListItem)
                        | Some(StructRole::ListItemLabel)
                );
                let list_item_changed = is_list_item_role != prev_was_list_item;

                // Tagged-PDF block boundary (issue #377 D5): adjacent
                // spans whose nearest paragraph-level structure ancestor
                // differs are explicitly separate paragraphs even when
                // the geometric gap is small (pdfa_049 has body-tight
                // inter-paragraph gaps that the gap heuristic never
                // catches).
                //
                // D5b refinement: gate this on `!same_line` so a tagged
                // form whose horizontal heading band is split into
                // multiple /P sub-elements on one line (irs_f1040 has
                // `Form` + `1040` + `U.S. Individual Income Tax Return`
                // as three sibling /P blocks at the same y) does not
                // become three separate `# Form` / `# 1040` / ... lines.
                //
                // D5c refinement: when same_line is true but a
                // multi-column gutter separates the spans (large
                // horizontal gap), restore the break. Newspapers
                // (IA_0047) and other multi-column tagged docs
                // otherwise produce concatenated tokens like
                // `andmight` from adjacent-column content sharing a
                // baseline.
                let column_gap = is_column_gap(prev, span);
                let line_truly_continuous = same_line && !column_gap;
                let block_changed = match (span.block_id, prev.block_id) {
                    (Some(a), Some(b)) => a != b,
                    _ => false,
                } && !line_truly_continuous;

                // For heading transitions: same logic — visual line
                // continuity wins over structure-tree fragmentation.
                // For list-item transitions: ALWAYS break because a
                // bullet `- ` needs its own markdown line regardless
                // of whether the source PDF rendered the marker
                // inline with a leading caption.
                let heading_changed_break = heading_changed && !line_truly_continuous;

                // Structure-authoritative paragraph reflow (ISO 32000-1 §14.8.3:
                // one `<P>` BLSE is a single paragraph that "can be split
                // between lines of text"). When two spans share a paragraph
                // block, the *geometric* gap heuristic must not split a wrapped
                // line — but only when this is genuinely a mid-sentence wrap.
                // The continuation signals (cheap, language-neutral) keep
                // intentional same-block line breaks intact: form fields and
                // record rows start with a capitalised label, code is tagged
                // preformatted, and a sentence end is a hard boundary.
                let same_block = matches!(
                    (span.block_id, prev.block_id),
                    (Some(a), Some(b)) if a == b
                );
                let plain_para = current_heading_level.is_none()
                    && span_heading_level.is_none()
                    && !is_list_item_role
                    && !prev_was_list_item
                    && !span.preformatted
                    && !prev.preformatted;
                // The next line continues the sentence when it starts lowercase
                // (`…downtown by\nnine…`), with a caseless-script letter
                // (Hebrew/Arabic/CJK/Indic have no capitalisation, so a letter
                // at the line start is always a continuation, never a new
                // sentence/field), or with a number that is not an ordered-list
                // marker (`…downtown by\n2028`). An ordered marker (`2.` / `3)`)
                // is a deliberate new item and must not merge.
                let next_trim = span.span.text.trim_start();
                let next_continues_lowercase = next_trim.chars().next().is_some_and(|c| {
                    c.is_lowercase()
                        || (c.is_alphabetic() && !c.is_uppercase())
                        || (c.is_ascii_digit()
                            && super::is_ordered_list_marker(next_trim).is_none())
                });
                let prev_trimmed = current_line.trim_end();
                let prev_unterminated = prev_trimmed
                    .chars()
                    .last()
                    .is_none_or(|c| !matches!(c, '.' | '!' | '?' | ':' | ';'));
                // A line broken mid-word at a hyphen (`…three sub-\nNeptune…`)
                // is always a wrap, even when the continuation is capitalised (a
                // proper noun) — the trailing hyphen is the continuation signal.
                let next_continues_lowercase =
                    next_continues_lowercase || prev_trimmed.ends_with('-');
                // The previous line must have run to the column margin — a
                // genuine wrap. A short ragged line (shell command, mis-tagged
                // code, a record value) is a deliberate break and is preserved.
                // Left-to-right lines fill toward the RIGHT margin; right-to-left
                // lines (Arabic/Hebrew) start at the right and fill toward the
                // LEFT margin, so the test is mirrored for them.
                let tol = prev.span.font_size.max(8.0) * 1.5;
                let prev_is_rtl = crate::text::bidi::looks_rtl(&prev.span.text);
                let prev_fills_column = prev.block_id.is_some_and(|b| {
                    if prev_is_rtl {
                        block_left_min
                            .get(&b)
                            .is_some_and(|&lo| prev.span.bbox.x <= lo + tol)
                    } else {
                        block_right_max
                            .get(&b)
                            .is_some_and(|&hi| prev.span.bbox.x + prev.span.bbox.width >= hi - tol)
                    }
                });
                let merge_wrapped_line = same_block
                    && plain_para
                    && next_continues_lowercase
                    && prev_unterminated
                    && prev_fills_column;
                // A genuine intra-paragraph wrap (same block, plain body text,
                // previous line filled the column margin, next line continues)
                // is one paragraph, so it suppresses every *soft* break — the
                // geometric gap, a reading-order group change, and a structure
                // block change (the same `<P>` re-entered). It does NOT override
                // a list-item transition or a real column gutter.
                let soft_break = group_flush
                    || self.is_paragraph_break(span, prev)
                    || heading_changed_break
                    || block_changed;
                if (soft_break && !merge_wrapped_line) || list_item_changed || column_gap {
                    close_formatting(&mut current_line, &mut active_bold, &mut active_italic);
                    if !current_line.is_empty() {
                        if let Some(level) = current_heading_level {
                            let prefix = "#".repeat(level as usize);
                            result.push_str(&format!(
                                "{} {}\n\n",
                                prefix,
                                strip_emphasis(current_line.trim())
                            ));
                        } else {
                            push_plain_paragraph(
                                &mut result,
                                current_line.trim(),
                                current_line_all_mono,
                            );
                        }
                        current_line.clear();
                    }
                    current_heading_level = span_heading_level;
                    if is_list_item_role && !span_is_ordered {
                        Self::push_list_marker(&mut current_line, span.span.bbox.x, &list_x_levels);
                    }
                } else if !same_line {
                    // Different visual line but within paragraph spacing.
                    // Check if a bullet or ordered-marker item starts here
                    // — if so, start a new line. Issue #377 D3 guards
                    // numbered lists (`1. Foo` / `2. Bar` / `3. Baz`) at
                    // the same X across consecutive baselines: the items
                    // must not concatenate into one line of running text.
                    //
                    // Only fire on a list-item *transition* (the body of
                    // a wrapped LI keeps the same role across visual
                    // lines and must NOT emit a fresh bullet on each
                    // wrapped line).
                    let is_bullet = Self::is_bullet_span(&span.span.text)
                        || Self::starts_with_bullet(&span.span.text);
                    let is_ordered =
                        Self::is_ordered_list_marker(span.span.text.trim_start()).is_some();
                    // Tagged docs: each /LI gets its own `block_id`,
                    // so wrapped multi-line items share the same id
                    // and we should only fire on a TRANSITION
                    // (different block_id or list_item_changed).
                    // Untagged docs (block_id None on both): can't
                    // tell which body lines are wrapped vs which are
                    // new items, so fall back to "any list-role on a
                    // new baseline starts a new item".
                    let starts_new_list_item = if span.block_id.is_some() && prev.block_id.is_some()
                    {
                        is_list_item_role && (list_item_changed || block_changed)
                    } else {
                        is_list_item_role
                    };
                    if is_bullet || is_ordered || starts_new_list_item {
                        // Bullet on new line → flush current line and start list item
                        close_formatting(&mut current_line, &mut active_bold, &mut active_italic);
                        if !current_line.is_empty() {
                            if let Some(level) = current_heading_level {
                                let prefix = "#".repeat(level as usize);
                                result.push_str(&format!(
                                    "{} {}\n\n",
                                    prefix,
                                    strip_emphasis(current_line.trim())
                                ));
                            } else {
                                result.push_str(Self::flush_line(&current_line));
                                result.push('\n');
                            }
                            current_line.clear();
                        }
                        current_heading_level = span_heading_level;
                        if starts_new_list_item && !span_is_ordered {
                            Self::push_list_marker(
                                &mut current_line,
                                span.span.bbox.x,
                                &list_x_levels,
                            );
                        }
                    } else {
                        // Different visual line within the same paragraph — close
                        // open formatting before the line-join space so that
                        // formatting is re-evaluated for the new line's spans.
                        close_formatting(&mut current_line, &mut active_bold, &mut active_italic);
                        if config.output.preserve_layout {
                            let spacing = (span.span.bbox.x - prev.span.bbox.x).max(0.0) as usize;
                            for _ in 0..spacing.min(20) {
                                current_line.push(' ');
                            }
                        } else {
                            // A line wrapped mid-word at a hyphen (`frozen-` /
                            // `thawed`) joins WITHOUT a space — `frozen- thawed`
                            // (a space after the hyphen) is never correct.
                            let trimmed = current_line.trim_end();
                            if !trimmed.ends_with('-') {
                                current_line.push(' ');
                            } else if crate::extractors::text::splits_one_word(
                                trimmed,
                                span.span.text.trim_start(),
                            ) {
                                // The hyphen belongs to the line break rather than the
                                // word, so it goes too: `implementa-` / `tion` is
                                // `implementation`. The guard keeps it wherever it may
                                // be the author's — capitals, digits, and compounds
                                // such as `pre-training` — which is the ambiguity this
                                // site previously resolved by always keeping it.
                                current_line.truncate(trimmed.len() - 1);
                            }
                        }
                    }
                }
            } else {
                current_heading_level = span_heading_level;
                if is_list_item_role && !span_is_ordered {
                    Self::push_list_marker(&mut current_line, span.span.bbox.x, &list_x_levels);
                }
            }

            // Standalone bullet-glyph span → markdown list marker.
            if Self::is_bullet_span(&span.span.text) {
                if !current_line.ends_with("- ") {
                    if !current_line.is_empty() && !current_line.ends_with(' ') {
                        current_line.push(' ');
                    }
                    Self::push_list_marker(&mut current_line, span.span.bbox.x, &list_x_levels);
                }
                prev_span = Some(span);
                continue;
            }

            // Apply column-spanning-decimal / char_widths-boundary split
            // (issue 487 nougat_018).  Mirrors `push_span_text` in the text
            // extractor so sailing-score cells like "1.10" (sparse cw,
            // really `1` + `10` in adjacent columns) split into two tokens
            // for markdown output too.
            let mut text_str = String::new();
            crate::document::PdfDocument::push_span_text(&mut text_str, &span.span);

            // Confirmed inline footnote marker: replace the raw superscript
            // token (e.g. "1" / "*") with markdown footnote-reference
            // syntax `[^N]`. It then flows through the normal same-line
            // append path and glues onto the preceding body text.
            if let Some(label) = footnote_marker_label {
                text_str = label.clone();
            }

            // Normalize known mis-extracted bullet glyphs (DEL from Zapf
            // Dingbats mappings, ❍ from ligature remaps) to U+2022 so the
            // bullet-span logic above can recognize them uniformly.
            //
            // POSITION-AWARE (issue #13 / user-content-preservation
            // principle): only replace the FIRST occurrence when it
            // sits at the very start of the span (a bullet position).
            // Mid-prose `❍` / DEL must survive verbatim — if the
            // source PDF actually contains those codepoints in body
            // text, rewriting them is content corruption. Bullet
            // detection at line start is intact; arbitrary text-stream
            // codepoints are no longer mutated.
            let trim_start = text_str.trim_start();
            if let Some(first) = trim_start.chars().next() {
                if first == '\x7f' || first == '❍' {
                    let leading_ws_len = text_str.len() - trim_start.len();
                    // Replace just this leading char, leave any later
                    // occurrences inside the same span verbatim.
                    let bullet_byte_len = first.len_utf8();
                    text_str = format!(
                        "{}•{}",
                        &text_str[..leading_ws_len],
                        &text_str[leading_ws_len + bullet_byte_len..]
                    );
                }
            }

            // Pipe characters are only markdown-syntactic inside table
            // cells; in paragraph flow they are just text. Pipe escaping
            // for tables is handled in render_table_markdown. Leaving `|`
            // alone in flow avoids showing `&#124;` in user-visible prose.

            let mut text = text_str.as_str();

            // Handle inline bullets (text starts with bullet char)
            if Self::starts_with_bullet(text) {
                let stripped = Self::strip_bullet(text);
                if !current_line.ends_with("- ") {
                    if !current_line.is_empty() && !current_line.ends_with(' ') {
                        current_line.push(' ');
                    }
                    Self::push_list_marker(&mut current_line, span.span.bbox.x, &list_x_levels);
                }
                text = stripped;
            }

            let normalized;
            if !config.output.preserve_layout {
                // In PDFs, adjacent spans on the same line often have slightly
                // overlapping bboxes (negative horizontal gap) with the inter-span
                // whitespace encoded as leading/trailing spaces in the span text
                // itself.  normalize_whitespace collapses internal runs of spaces
                // but would also strip these boundary spaces, causing words from
                // neighbouring spans to merge (e.g. "visitwww.example.comto").
                // Preserve a leading space when a same-line predecessor exists and
                // a trailing space unconditionally so the next span can abut
                // correctly.  The plain-text converter avoids this problem by
                // skipping per-span normalization entirely.
                let had_leading_space =
                    same_line && prev_span.is_some() && text.starts_with(char::is_whitespace);
                let had_trailing_space = text.ends_with(char::is_whitespace);
                let mut norm = self.normalize_whitespace(text);
                if had_leading_space && !norm.starts_with(' ') {
                    norm.insert(0, ' ');
                }
                if had_trailing_space && !norm.ends_with(' ') && !norm.is_empty() {
                    norm.push(' ');
                }
                normalized = norm;
                text = &normalized;
            }

            // A span inside a /Link annotation becomes a markdown link
            // `[text](uri)` — but only for safe schemes (reject javascript:,
            // data:, … to avoid injecting active content). Otherwise fall back
            // to autolinking bare URLs in the text.
            let linkified = match span.link_uri.as_deref() {
                Some(uri) if super::is_safe_link_uri(uri) => {
                    format!(
                        "[{}]({})",
                        text.trim(),
                        uri.replace(' ', "%20").replace(')', "%29")
                    )
                }
                _ => self.linkify(text),
            };

            let is_bold = self.is_bold(span, config);
            let is_italic = self.is_italic(span);

            // Issue #260: Detect horizontal gaps between same-line spans and
            // insert a space.  PDFs generated by PDFKit.NET (and similar) place
            // each word in its own BT/ET block with absolute positioning.  The
            // spans carry no leading/trailing whitespace so the PR #273
            // whitespace-preservation logic above cannot help.  We replicate the
            // same gap heuristic used by extract_text()'s should_insert_space():
            // gap > 15% of font size → space, but not if > 5× font size (column
            // boundary).
            if same_line && !current_line.is_empty() {
                if let Some(prev) = prev_span {
                    let no_existing_ws =
                        !current_line.ends_with(' ') && !linkified.starts_with(' ');
                    // Visual gap heuristic (issue #260).
                    let visual_gap = super::has_horizontal_gap(&prev.span, &span.span);
                    // Punctuation/case heuristic: when prev ends in a sentence
                    // boundary (`.`, `,`, `;`, `:`, `?`, `!`) and the next span
                    // begins with an uppercase letter or digit, it's overwhelmingly
                    // likely a missing space — even if the bbox gap is below the
                    // visual threshold (tightly typeset academic PDFs are common
                    // offenders, producing text like "methods.The financial...").
                    let punct_boundary = current_line
                        .chars()
                        .last()
                        .is_some_and(|c| matches!(c, '.' | ',' | ';' | ':' | '?' | '!'))
                        && linkified
                            .chars()
                            .next()
                            .is_some_and(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
                    if no_existing_ws && (visual_gap || punct_boundary) {
                        current_line.push(' ');
                    }
                }
            }

            // Consolidate adjacent spans with the same formatting style into
            // a single bold/italic block instead of wrapping each span
            // individually (e.g. **ACME GLOBAL LTD.** not
            // **ACME** **GLOBAL** **LTD.**).
            //
            // When the formatting changes we close the old markers and open
            // new ones.  When it stays the same we just append the text.
            if is_bold != active_bold || is_italic != active_italic {
                // Close previous formatting markers (if any)
                close_formatting(&mut current_line, &mut active_bold, &mut active_italic);
                // Open new markers
                if is_bold {
                    current_line.push_str("**");
                    active_bold = true;
                }
                if is_italic {
                    current_line.push('*');
                    active_italic = true;
                }
            }

            // Track whether the line being built is entirely monospace. At this
            // point `current_line` reflects the post-flush state (a break above
            // cleared it), so an empty / bullet-only line re-arms the flag before
            // this span's content lands; each non-blank span then narrows it.
            if current_line.trim_start_matches("- ").trim().is_empty() {
                current_line_all_mono = true;
            }
            if span.span.text.chars().any(|c| !c.is_whitespace()) {
                current_line_all_mono &= span.span.is_monospace;
            }

            current_line.push_str(&linkified);

            prev_span = Some(span);
        }

        // Close any open formatting before final flushes
        close_formatting(&mut current_line, &mut active_bold, &mut active_italic);

        // Recover orphaned spans: spans inside a table region whose text does
        // not appear in the rendered table output.
        for (table_idx, skipped) in table_skipped_spans.iter().enumerate() {
            if !tables_rendered[table_idx] || skipped.is_empty() {
                continue;
            }
            let rendered = &table_mds[table_idx];
            let mut orphans: Vec<&&OrderedTextSpan> = skipped
                .iter()
                .filter(|s| {
                    let trimmed = s.span.text.trim();
                    !trimmed.is_empty() && !rendered.contains(trimmed)
                })
                .collect();
            if !orphans.is_empty() {
                orphans.sort_by_key(|s| s.reading_order);
                for orphan in orphans {
                    if !result.ends_with(' ') && !result.ends_with('\n') {
                        result.push(' ');
                    }
                    // Apply column-spanning-decimal / char_widths-boundary
                    // split (issue 487 nougat_018): orphan score spans
                    // emitted as "25.10" with sparse cw split into "25 10".
                    let mut processed = String::new();
                    crate::document::PdfDocument::push_span_text(&mut processed, &orphan.span);
                    result.push_str(&processed);
                }
            }
        }

        // Render any tables that weren't matched to spans (e.g., all spans were in tables)
        for (i, table) in tables.iter().enumerate() {
            if !tables_rendered[i] && !table.is_empty() {
                if !current_line.is_empty() {
                    if let Some(level) = current_heading_level {
                        let prefix = "#".repeat(level as usize);
                        result.push_str(&format!(
                            "{} {}\n\n",
                            prefix,
                            strip_emphasis(current_line.trim())
                        ));
                    } else {
                        push_plain_paragraph(
                            &mut result,
                            current_line.trim(),
                            current_line_all_mono,
                        );
                    }
                    current_line.clear();
                }
                result.push_str(&table_mds[i]);
                result.push('\n');
            }
        }

        // Flush remaining content
        if !current_line.is_empty() {
            if let Some(level) = current_heading_level {
                let prefix = "#".repeat(level as usize);
                result.push_str(&format!(
                    "{} {}\n",
                    prefix,
                    strip_emphasis(current_line.trim())
                ));
            } else if current_line_all_mono {
                // Trailing monospace paragraph — sentinel-wrap so the fence pass
                // fuses it with any preceding code lines.
                result.push(MONO_SENTINEL);
                result.push_str(Self::flush_line(&current_line));
                result.push(MONO_SENTINEL);
                result.push('\n');
            } else {
                result.push_str(Self::flush_line(&current_line));
                result.push('\n');
            }
        }

        Ok(finalize_markdown(result, &sorted, config, &footnote_plan))
    }
}
