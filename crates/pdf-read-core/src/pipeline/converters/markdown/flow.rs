use super::*;

/// Detect a multi-column gutter between two spans on the same baseline.
///
/// Used by the markdown converter to refine its `same_line` gate: two
/// spans at the same y but separated by a large horizontal gap are
/// almost certainly in different columns (newspaper / two-column
/// academic paper). They must NOT be merged into one paragraph even
/// if their `block_id`s suggest a structural transition would be
/// suppressed by D5b.
///
/// Returns true in two distinct shapes (issue #377 D5d):
///
/// 1. **Forward column gap.** The horizontal gap from the right edge
///    of the previous span to the left edge of the current span
///    exceeds `max(3 × font_size, 30 pt)`. 3× font size catches
///    typical body-text columns (12pt body → 36pt gutter); the 30pt
///    floor catches small-font cases where a literal 36pt gap would
///    be too lenient.
///
/// 2. **Backward column wrap (x went backwards on the same baseline).**
///    LTR text on a single visual line always advances x forward; if
///    the current span starts to the left of the previous span by
///    more than `2 × font_size`, that is a column-major reading order
///    wrapping from the end of one column back to the top of the
///    next. The IA_0047 newspaper struct tree emits content this way:
///    `constitution` at x=976 ends a column, `Assailing` at x=192
///    starts the next, both at the same baseline. Without the
///    backward-wrap detection the converter joins them into the
///    nonsense token `constitutionAssailing`.
///
/// Marker wrapped around a fully-monospace plain paragraph at flush time so
/// `fence_monospace_blocks` can recognise and fuse consecutive ones. NUL never
/// occurs in extracted text, so it is unambiguous and is always removed.
pub(super) const MONO_SENTINEL: char = '\u{0}';

/// Emit a plain (non-heading, non-list) paragraph, wrapping it in
/// [`MONO_SENTINEL`] markers when it is entirely monospace so the fence pass
/// can fuse a run of them into one code block.
pub(super) fn push_plain_paragraph(result: &mut String, line: &str, mono: bool) {
    if mono && !line.is_empty() {
        result.push(MONO_SENTINEL);
        result.push_str(line);
        result.push(MONO_SENTINEL);
    } else {
        result.push_str(line);
    }
    result.push_str("\n\n");
}

/// Fuse consecutive monospace paragraphs (each flagged with [`MONO_SENTINEL`])
/// into a single fenced code block, preserving their internal line breaks.
/// A run of one or more sentinel-marked paragraphs becomes:
/// ```text
/// ```
/// line 1
/// line 2
/// ```
/// ```
/// Non-marked paragraphs pass through unchanged; the markers are always
/// stripped (so even a lone inline NUL, which never occurs anyway, is removed).
pub(super) fn fence_monospace_blocks(s: &str) -> String {
    if !s.contains(MONO_SENTINEL) {
        return s.to_string();
    }
    let paras: Vec<&str> = s.split("\n\n").collect();
    let mut out: Vec<String> = Vec::with_capacity(paras.len());
    let mut code: Vec<String> = Vec::new();
    let flush_code = |code: &mut Vec<String>, out: &mut Vec<String>| {
        if !code.is_empty() {
            let body = code.join("\n");
            // A fence around whitespace-only content carries no information and,
            // on a scanned or blank page whose only spans are stray whitespace,
            // it would make the page's Markdown non-empty. Drop it.
            if body.trim().is_empty() {
                code.clear();
                return;
            }
            out.push(format!("```\n{body}\n```"));
            code.clear();
        }
    };
    for p in paras {
        let trimmed = p.trim();
        if let Some(inner) = trimmed
            .strip_prefix(MONO_SENTINEL)
            .and_then(|t| t.strip_suffix(MONO_SENTINEL))
        {
            code.push(inner.to_string());
        } else {
            flush_code(&mut code, &mut out);
            // Defensive: strip any stray sentinel from a normal paragraph.
            if p.contains(MONO_SENTINEL) {
                out.push(p.replace(MONO_SENTINEL, ""));
            } else {
                out.push(p.to_string());
            }
        }
    }
    flush_code(&mut code, &mut out);
    out.join("\n\n")
}

pub(super) fn is_column_gap(prev: &OrderedTextSpan, current: &OrderedTextSpan) -> bool {
    let prev_right = prev.span.bbox.x + prev.span.bbox.width;
    let cur_left = current.span.bbox.x;
    let font_size = current.span.font_size.max(prev.span.font_size).max(1.0);

    // Right-to-left text reads with *decreasing* X, so the backward-X "column
    // wrap" heuristic below (which assumes left-to-right flow) is inverted for
    // it: an RTL line's spans, ordered into logical reading order, step left on
    // every word. Treating that as a column boundary splits the line into one
    // paragraph/heading per word (Arabic/Hebrew titles and prose). The RTL
    // reading-order pass has already placed these spans correctly, so skip the
    // backward-X branch when both sides are RTL.
    // A whitespace / neutral span between two RTL words is part of the RTL
    // flow (it has no direction of its own), so it must not break the RTL
    // context — otherwise every word→space→word step inside a right-to-left
    // line reads as a same-baseline column gap and the line shatters into one
    // paragraph per word. This covers both a pure-space separator and a short
    // neutral-punctuation one (e.g. " ," / " ." between two words): per UAX #9
    // such neutrals inherit the surrounding right-to-left direction, so a span
    // of ≤2 non-alphanumeric chars is RTL-friendly too.
    let rtl_friendly = |t: &str| {
        let s = t.trim();
        crate::text::bidi::looks_rtl(t)
            || s.is_empty()
            || (s.chars().count() <= 2 && !s.chars().any(|c| c.is_alphanumeric()))
    };
    let both_rtl = rtl_friendly(&prev.span.text)
        && rtl_friendly(&current.span.text)
        && (crate::text::bidi::looks_rtl(&prev.span.text)
            || crate::text::bidi::looks_rtl(&current.span.text));

    // Backward wrap: x went meaningfully backwards. This is the signature of
    // BOTH a within-column line wrap (X resets to the column's left margin) and
    // a genuine column transition (X jumps to the next column). They are told
    // apart by Y: a normal line wrap steps DOWN by about one line, whereas a
    // column transition jumps back UP to the next column's top (Y increases) or
    // drops far more than a line. Only the latter is a column boundary —
    // treating a plain wrap as one splits every wrapped line in a narrow column
    // into its own paragraph (multi-column newspaper / journal bodies).
    if !both_rtl && cur_left + font_size * 2.0 < prev.span.bbox.x {
        // A within-column line wrap drops by about ONE line; a column
        // transition either jumps back UP (Y increases), drops far more than a
        // line, or stays on roughly the SAME baseline (a balanced column whose
        // next column resumes at the same height). Only a ~one-line downward
        // step is an ordinary wrap.
        let y_drop = prev.span.bbox.y - current.span.bbox.y;
        if y_drop > font_size * 0.5 && y_drop < font_size * 2.0 {
            return false; // ordinary line wrap, not a column boundary
        }
        return true;
    }

    // Forward gutter: gap exceeds typical inter-word spacing.
    let gap = cur_left - prev_right;
    if gap <= 0.0 {
        return false;
    }
    let threshold = (font_size * 3.0).max(30.0);
    gap > threshold
}
