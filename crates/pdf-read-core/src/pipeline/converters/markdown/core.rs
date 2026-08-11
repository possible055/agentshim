use super::*;

impl MarkdownOutputConverter {
    /// Check if a span should be rendered as bold.
    pub(super) fn is_bold(&self, span: &OrderedTextSpan, config: &TextPipelineConfig) -> bool {
        use crate::pipeline::config::BoldMarkerBehavior;

        match span.span.font_weight {
            FontWeight::Bold | FontWeight::Black | FontWeight::ExtraBold | FontWeight::SemiBold => {
                match config.output.bold_marker_behavior {
                    BoldMarkerBehavior::Aggressive => true,
                    BoldMarkerBehavior::Conservative => {
                        // Only apply bold to content-bearing text
                        span.span.text.chars().any(|c| !c.is_whitespace())
                    }
                }
            }
            _ => false,
        }
    }

    /// Check if a span should be rendered as italic.
    pub(super) fn is_italic(&self, span: &OrderedTextSpan) -> bool {
        span.span.is_italic && span.span.text.chars().any(|c| !c.is_whitespace())
    }

    /// Apply linkification to text (URLs and emails).
    pub(super) fn linkify(&self, text: &str) -> String {
        // Quick pre-check: skip regex for spans that can't contain URLs or emails.
        // This avoids regex overhead for ~95% of regular text spans.
        let might_have_url = text.contains("://") || text.contains("www.");
        let might_have_email = text.contains('@');

        if !might_have_url && !might_have_email {
            return text.to_string();
        }

        let mut result = if might_have_url {
            RE_URL
                .replace_all(text, |caps: &regex::Captures| {
                    let url = &caps[0];
                    format!("[{}]({})", url, url)
                })
                .to_string()
        } else {
            text.to_string()
        };

        if might_have_email {
            result = RE_EMAIL
                .replace_all(&result, |caps: &regex::Captures| {
                    let email = &caps[0];
                    format!("[{}](mailto:{})", email, email)
                })
                .to_string();
        }

        result
    }

    /// Normalize whitespace in text.
    pub(super) fn normalize_whitespace(&self, text: &str) -> String {
        // Replace multiple spaces with single space
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// Detect paragraph breaks between spans based on vertical spacing.
    ///
    /// Two break signals:
    /// 1. Vertical gap larger than `paragraph_gap_ratio × line_height`
    ///    (the classic geometric heuristic).
    /// 2. The current line begins with a list marker (bullet glyph or
    ///    ordered marker) while the previous line did not — list-items
    ///    must always start a fresh paragraph regardless of how tightly
    ///    they sit under the preceding paragraph (issue #377 D4: many
    ///    untagged docs use a sub-1.5× line gap before lists, which
    ///    glues the first item to the intro sentence).
    pub(super) fn is_paragraph_break(
        &self,
        current: &OrderedTextSpan,
        previous: &OrderedTextSpan,
    ) -> bool {
        let line_height = current.span.font_size.max(previous.span.font_size);
        let gap = (previous.span.bbox.y - current.span.bbox.y).abs();
        if gap > line_height * self.paragraph_gap_ratio {
            return true;
        }
        // List-prefix transition guard. Bullet glyph or `1.` / `a)` /
        // `i.` ordered marker at the start of the current line, with
        // the previous line on a different baseline and not itself a
        // list item. The ordered-marker detection is conservative
        // (single digit/letter at line start) so figure captions
        // ("1.1 Foo") and years ("1986") are not promoted to lists.
        let line_changed =
            (previous.span.bbox.y - current.span.bbox.y).abs() > current.span.font_size * 0.5;
        if line_changed {
            let cur_text = current.span.text.trim_start();
            let cur_starts_list = Self::is_bullet_span(cur_text)
                || Self::starts_with_bullet(cur_text)
                || Self::is_ordered_list_marker(cur_text).is_some();
            let prev_text = previous.span.text.trim_start();
            let prev_starts_list = Self::is_bullet_span(prev_text)
                || Self::starts_with_bullet(prev_text)
                || Self::is_ordered_list_marker(prev_text).is_some();
            if cur_starts_list && !prev_starts_list {
                return true;
            }
        }
        false
    }

    /// Detect a markdown ordered-list marker at the start of `text`.
    /// Recognises `1.`, `12.`, `a.`, `iv.`, `1)`, `a)` followed by a
    /// space. Returns the (1-based) position number when known
    /// (Roman numerals coerced to position 1 for now), or `None`.
    ///
    /// Conservative on purpose — only single digit/letter tokens at
    /// the very start of the trimmed text qualify, so figure captions
    /// like "1.1 Foo" and years like "1986" are not falsely promoted
    /// to numbered lists. See issue #377 D3.
    pub(super) fn is_ordered_list_marker(text: &str) -> Option<u32> {
        super::is_ordered_list_marker(text)
    }

    /// Check if a span consists of a single bullet character.
    ///
    /// Common bullet characters used in PDF documents:
    /// ► • ▪ ▸ ‣ ◦ ● ■ ◆ ○ □ ❍ ❖ ✓ ✔ ➢ ➤ 
    pub(super) fn is_bullet_span(text: &str) -> bool {
        super::is_bullet_span(text)
    }

    /// Check if text starts with a bullet character (for inline bullets).
    pub(super) fn starts_with_bullet(text: &str) -> bool {
        super::starts_with_bullet(text)
    }

    /// Validate that a string looks like a heading (not a paragraph or noise).
    ///
    /// Content-based guards only — no language/locale-specific keyword lists.
    pub(super) fn is_valid_heading_text(text: &str) -> bool {
        let trimmed = text.trim();
        let text_len = trimmed.chars().count();
        // Headings must be non-trivial but also not full paragraphs.
        // 200 chars is ~35 words, which safely accommodates long wrapped titles
        // while excluding paragraph-length runs that share a larger font.
        if !(2..=200).contains(&text_len) {
            return false;
        }
        // Reject a bare ordinal suffix (`st`/`nd`/`rd`/`th`) left stranded when a
        // superscript ordinal is split from its number ("May 5th" → "May 5" +
        // superscript "th"). On its own it is never a heading; promoting it emits
        // a stray "#### th" that fragments the document outline.
        if matches!(
            trimmed.to_ascii_lowercase().as_str(),
            "st" | "nd" | "rd" | "th"
        ) {
            return false;
        }
        // Sentence-length guards: a heading rarely exceeds 20 words and
        // almost never contains a full stop followed by more text (that's
        // a paragraph, even if it happens to be set in a larger font).
        let word_count = trimmed.split_whitespace().count();
        if word_count > 20 {
            return false;
        }
        // Exclude runs with mid-sentence punctuation ("foo. Bar baz") —
        // real headings don't contain sentence boundaries.
        let bytes = trimmed.as_bytes();
        for i in 0..bytes.len().saturating_sub(2) {
            if bytes[i] == b'.' && bytes[i + 1] == b' ' {
                let next = bytes[i + 2];
                if next.is_ascii_alphabetic() {
                    return false;
                }
            }
        }

        // Reject if dominated by digits/punctuation (KPI numbers, page numbers,
        // "$100", "23.5K"). Require a minimum alphabetic ratio that scales:
        // very short strings need at least 2 letters; longer strings need
        // >=30% alphabetic characters.
        let alpha_count = trimmed.chars().filter(|c| c.is_alphabetic()).count();
        if text_len <= 8 {
            if alpha_count < 2 {
                return false;
            }
        } else if alpha_count * 10 < text_len * 3 {
            return false;
        }

        // Reject KPI-style values ("4.2 days", "+15% QoQ", "$1.2M Total"):
        // strings that LEAD with a number/sign/currency symbol are almost
        // always data values, not headings, even in a larger font. A real
        // heading leads with a word.
        let first = trimmed.chars().next().unwrap_or(' ');
        if first.is_ascii_digit() || matches!(first, '+' | '-' | '$' | '€' | '£' | '¥' | '%') {
            return false;
        }

        // Reject leader/rule/separator artifacts: a run of 4+ identical fill
        // characters (dot leaders, dashed rules, table separators) is TOC/table
        // furniture, never a heading — e.g. a font-listing row
        // "Embedded font--------------------Bavhavs" that a font-size tier would
        // otherwise promote (172 spurious `#` observed on one font-sample doc).
        {
            let mut run = 1usize;
            let mut prev = '\0';
            for c in trimmed.chars() {
                run = if c == prev && matches!(c, '-' | '_' | '.' | '·' | '—' | '=' | '*') {
                    run + 1
                } else {
                    1
                };
                if run >= 4 {
                    return false;
                }
                prev = c;
            }
        }

        // Reject a line ending in a hyphen: that is a mid-word line break
        // ("...three categories: com-"), i.e. a wrapped body line, not a heading.
        if trimmed.ends_with('-') {
            return false;
        }

        // Reject a lowercase-initial multi-word run: a real multi-word heading
        // leads with a capital; a lowercase start on 5+ words is a body
        // continuation wrongly caught by a font-size tier. (CJK/Arabic have no
        // case, so `is_lowercase()` is false there and those headings pass.)
        if word_count >= 5 {
            if let Some(f) = trimmed.chars().find(|c| c.is_alphabetic()) {
                if f.is_lowercase() {
                    return false;
                }
            }
        }

        // Title-case vs sentence-case: this is the line-shape signal that a
        // pure font-size heuristic (pymupdf4llm) lacks and an ML layout model
        // (marker) captures. A long line whose Latin-script words are mostly
        // lowercase is flowing prose — body text glued onto a promoted span —
        // not a heading, however large the font. Real multi-word headings are
        // title-cased (or short, <=8 words, which are exempt). Only cased-script
        // words are counted, so CJK/Arabic/numeric headings are unaffected.
        if word_count > 8 {
            let cased: Vec<char> = trimmed
                .split_whitespace()
                .filter_map(|w| w.chars().next())
                .filter(|c| c.is_uppercase() || c.is_lowercase())
                .collect();
            if !cased.is_empty() {
                let upper = cased.iter().filter(|c| c.is_uppercase()).count();
                // < 40% of cased words capitalised → sentence-case body.
                if upper * 5 < cased.len() * 2 {
                    return false;
                }
            }
        }

        true
    }

    /// Re-validate every emitted heading against its COMPLETE line, demoting
    /// (dropping the `#` prefix) any that no longer reads as a heading.
    ///
    /// Heading level is decided per span (before the line's spans are
    /// concatenated), so a short span that passed [`Self::is_valid_heading_text`]
    /// can accrete a body continuation into a long, sentence-case line. This
    /// applies the same gate at line granularity — the block-level view an ML
    /// layout model would take. Demotion-only: a genuine heading always passes,
    /// so this can never remove a real heading nor create a false one.
    pub(super) fn demote_body_like_headings(md: &str) -> String {
        let mut out = String::with_capacity(md.len());
        for (i, line) in md.split('\n').enumerate() {
            if i > 0 {
                out.push('\n');
            }
            let hashes = line.bytes().take_while(|&b| b == b'#').count();
            if (1..=6).contains(&hashes) && line.as_bytes().get(hashes) == Some(&b' ') {
                let text = line[hashes + 1..].trim();
                if !Self::is_valid_heading_text(text) {
                    // Demote to a plain paragraph line (keep the text verbatim).
                    out.push_str(line[hashes + 1..].trim_start());
                    continue;
                }
            }
            out.push_str(line);
        }
        out
    }

    /// Distinct left-edge x-positions of list-marker spans, ascending,
    /// collapsed to ~3pt buckets. A list item's nesting depth (WS2.5) is the
    /// number of these levels strictly to its left. A flat list yields one
    /// level, so every item is depth 0 and its markdown is byte-identical to
    /// the pre-nesting output — inert unless a document genuinely indents.
    pub(super) fn list_marker_x_levels(spans: &[&OrderedTextSpan]) -> Vec<f32> {
        let mut xs: Vec<f32> = spans
            .iter()
            .filter(|s| {
                let t = s.span.text.trim_start();
                Self::is_bullet_span(&s.span.text)
                    || Self::starts_with_bullet(&s.span.text)
                    || Self::is_ordered_list_marker(t).is_some()
            })
            .map(|s| s.span.bbox.x)
            .collect();
        xs.sort_by(|a, b| crate::utils::safe_float_cmp(*a, *b));
        let mut levels: Vec<f32> = Vec::new();
        for x in xs {
            if levels.last().is_none_or(|&l| x - l > 3.0) {
                levels.push(x);
            }
        }
        levels
    }

    /// Two-space markdown indent for a list marker at `x` (two spaces per level
    /// to its left; empty at the leftmost level).
    pub(super) fn list_indent(x: f32, levels: &[f32]) -> String {
        "  ".repeat(levels.iter().filter(|&&l| x > l + 3.0).count())
    }

    /// Append a `- ` list marker to `line`, prefixed with the WS2.5 nesting
    /// indent when `line` is empty (i.e. at the start of a fresh list line —
    /// mid-line bullets are left unindented). Flat lists yield an empty indent,
    /// keeping output byte-identical to before.
    pub(super) fn push_list_marker(line: &mut String, x: f32, levels: &[f32]) {
        if line.is_empty() {
            line.push_str(&Self::list_indent(x, levels));
        }
        line.push_str("- ");
    }

    /// Flush-time trim that preserves a list item's leading indent. Identical
    /// to `str::trim()` for every non-list line, but keeps leading spaces when
    /// the trimmed content is a markdown list marker — so WS2.5 nesting
    /// survives the line flush. Inert on flat lists (their indent is empty).
    pub(super) fn flush_line(s: &str) -> &str {
        let te = s.trim_end();
        if te.trim_start().starts_with("- ") {
            te
        } else {
            te.trim_start()
        }
    }

    /// Strip the leading bullet character from text, returning the rest.
    pub(super) fn strip_bullet(text: &str) -> &str {
        let t = text.trim_start();
        // Bullet characters are single Unicode code points; skip first char
        if Self::starts_with_bullet(t) {
            let mut chars = t.chars();
            chars.next(); // skip bullet
            chars.as_str().trim_start()
        } else {
            text
        }
    }

    /// Detect heading level from the span's font size relative to the
    /// document's body size (caller-provided, typically the mode of
    /// observed sizes). Ratios: H1 >=1.8x, H2 >=1.4x, H3 >=1.2x, or
    /// H4 for bold at >=1.05x.
    ///
    /// The bold-threshold tier exists for documents whose section
    /// headings are set in the same family as body text but bumped by
    /// only a few percent of point size — common in corporate manuals
    /// (issue #377 D2: amt_handbook_sample, nougat_032, technical
    /// docs). Without the bold gate this would over-promote
    /// emphasised inline phrases.
    pub(super) fn heading_level_ratio(
        &self,
        span: &OrderedTextSpan,
        base_font_size: f32,
    ) -> Option<u8> {
        if !Self::is_valid_heading_text(span.span.text.trim()) {
            return None;
        }
        if base_font_size <= 0.0 {
            return None;
        }
        let size_ratio = span.span.font_size / base_font_size;
        let is_bold = matches!(
            span.span.font_weight,
            FontWeight::Bold | FontWeight::Black | FontWeight::ExtraBold | FontWeight::SemiBold
        );
        if size_ratio >= 1.8 {
            Some(1)
        } else if size_ratio >= 1.4 {
            Some(2)
        } else if size_ratio >= 1.2 {
            Some(3)
        } else if is_bold && size_ratio >= 1.05 {
            // Bold text with even slight size increase is a heading signal.
            // H4 (was H3) since the weaker signal warrants a lower level.
            Some(4)
        } else {
            None
        }
    }

    /// WS2.4: promote a multi-level numbered section title ("2.1.3 Results")
    /// to a heading whose level is its dot-depth (2.1.3 → H3), capped at H6.
    /// Only DOTTED patterns (≥1 dot) qualify — a bare "1. " leads an ordered
    /// list item, and a bare "2 Foo" is too ambiguous — so this never steals
    /// from the list path. Used only as a FALLBACK when the font-size heuristic
    /// finds no heading, so it is purely additive: it adds section headings a
    /// same-font-size numbered document would otherwise flatten, and never
    /// changes a size-derived level.
    pub(super) fn numbered_heading_level(text: &str) -> Option<u8> {
        let t = text.trim_start();
        // Leading "N.N(.N)*" then whitespace then a non-digit title word.
        let mut chars = t.char_indices().peekable();
        let mut dots = 0u8;
        let mut saw_digit = false;
        let mut end = 0usize;
        while let Some(&(i, c)) = chars.peek() {
            if c.is_ascii_digit() {
                saw_digit = true;
                end = i + 1;
                chars.next();
            } else if c == '.' {
                // A trailing "1. " (dot then space) is an ordered-list marker,
                // not a section number — require another digit after the dot.
                let after = t[i + 1..].chars().next();
                if after.is_some_and(|n| n.is_ascii_digit()) {
                    dots += 1;
                    end = i + 1;
                    chars.next();
                } else {
                    break;
                }
            } else {
                break;
            }
        }
        if !saw_digit || dots == 0 {
            return None;
        }
        // Require a real title after the number (whitespace then a letter).
        let rest = t[end..].trim_start();
        let starts_alpha = rest.chars().next().is_some_and(|c| c.is_alphabetic());
        if !starts_alpha || rest.len() < 2 || rest.len() > 120 {
            return None;
        }
        Some((dots + 1).min(6))
    }
}
