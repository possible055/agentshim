use super::*;

/// A footnote reference token: either a run of digits (`1`, `12`) or a
/// single footnote symbol (`*`, `†`, `‡`, `§`, `¶`). Markers and
/// definitions are matched by the equality of these tokens.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(super) enum FootnoteToken {
    /// Numeric marker; stored as the raw digit string so `[^12]` keeps
    /// the author's number.
    Number(String),
    /// Symbol marker (`*`/`†`/`‡`/`§`/`¶`).
    Symbol(char),
}

impl FootnoteToken {
    /// String used to match a marker against a definition (`"1"`, `"*"`).
    fn key(&self) -> String {
        match self {
            FootnoteToken::Number(s) => s.clone(),
            FootnoteToken::Symbol(c) => c.to_string(),
        }
    }
}

/// The result of footnote detection over one page's spans.
///
/// All index fields are positions into the `sorted` slice passed to
/// `detect_footnotes` (which is exactly the enumeration index used by
/// the render loop). Every collection is empty when no footnote was
/// confirmed, in which case rendering is byte-for-byte unchanged.
#[derive(Default)]
pub(super) struct FootnotePlan {
    /// span index → inline replacement text (`"[^1]"`).
    pub(super) inline_markers: std::collections::HashMap<usize, String>,
    /// span indices belonging to a confirmed definition block (suppressed
    /// from normal body rendering).
    pub(super) definition_spans: std::collections::HashSet<usize>,
    /// assembled definition lines (`"[^1]: Smith et al. 2019"`), ordered.
    pub(super) definitions: Vec<String>,
}

/// Parse a whole span's text as a footnote *marker* token. Accepts a run
/// of 1–3 digits or a single footnote symbol; anything else (letters, an
/// ordinal suffix like `th`, multi-symbol runs) yields `None`.
pub(super) fn parse_footnote_marker_token(text: &str) -> Option<FootnoteToken> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    if t.chars().count() == 1 {
        let c = t.chars().next().unwrap();
        if matches!(c, '*' | '†' | '‡' | '§' | '¶') {
            return Some(FootnoteToken::Symbol(c));
        }
    }
    if t.len() <= 3 && t.chars().all(|c| c.is_ascii_digit()) {
        return Some(FootnoteToken::Number(t.to_string()));
    }
    None
}

/// Parse a page-bottom definition line: a leading footnote token, an
/// optional single `.`/`)`/`:` separator, then the definition text. The
/// marker must be followed by whitespace or a separator (so `1st` or
/// `12x` do not falsely parse), and the remaining definition must be
/// non-empty.
pub(super) fn split_leading_footnote_def(line: &str) -> Option<(FootnoteToken, String)> {
    let t = line.trim_start();
    let first = t.chars().next()?;
    let (token, consumed) = if matches!(first, '*' | '†' | '‡' | '§' | '¶') {
        (FootnoteToken::Symbol(first), first.len_utf8())
    } else if first.is_ascii_digit() {
        let digits: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.len() > 3 {
            return None;
        }
        let len = digits.len();
        (FootnoteToken::Number(digits), len)
    } else {
        return None;
    };
    let after = &t[consumed..];
    // Boundary: marker must be delimited from the definition text.
    if !(after.starts_with(char::is_whitespace) || after.starts_with(['.', ')', ':'])) {
        return None;
    }
    let rest = after.strip_prefix(['.', ')', ':']).unwrap_or(after);
    let def = rest.trim();
    if def.is_empty() {
        return None;
    }
    // A footnote definition is prose (a citation or note), not a stray numeric
    // run. Require a few letters so a math/table row like "2 2 1 2" — which
    // happens to lead with a digit token — is not mistaken for a definition.
    // Because a footnote is only emitted when a marker has a MATCHING
    // definition, rejecting the garbage def also suppresses the spurious inline
    // `[^n]` marker (the false-positive seen on math-heavy arXiv text).
    if def.chars().filter(|c| c.is_alphabetic()).count() < 3 {
        return None;
    }
    Some((token, def.to_string()))
}

/// Find the adjacent body span for a candidate superscript marker at
/// index `i`: the immediate predecessor or successor whose text carries a
/// letter (real prose, not another marker), preferring the larger font.
pub(super) fn adjacent_body_span(spans: &[&OrderedTextSpan], i: usize) -> Option<usize> {
    let candidates = [i.checked_sub(1), (i + 1 < spans.len()).then_some(i + 1)];
    let mut best: Option<usize> = None;
    for c in candidates.into_iter().flatten() {
        if !spans[c].span.text.chars().any(|ch| ch.is_alphabetic()) {
            continue;
        }
        best = match best {
            Some(b) if spans[b].span.font_size >= spans[c].span.font_size => Some(b),
            _ => Some(c),
        };
    }
    best
}

/// Conservative, high-precision footnote detection (WS2.7).
///
/// A footnote is only emitted when BOTH halves are found and their tokens
/// match: (a) an inline superscript marker in body text — a small, raised
/// digit/symbol span — AND (b) a page-bottom definition line starting with
/// the same token in a smaller-than-body font. Missing either half leaves
/// the text untouched (a false footnote is worse than a missed one).
///
/// Thresholds (chosen for precision): a marker font must be < 0.75× its
/// adjacent body span and its baseline raised by > 0.1× that body font;
/// the definition band is the bottom 18% of the page's baseline range and
/// its leading span must be < 0.92× the body font.
pub(super) fn detect_footnotes(spans: &[&OrderedTextSpan], base_font_size: f32) -> FootnotePlan {
    use std::collections::{HashMap, HashSet};
    let mut plan = FootnotePlan::default();
    if spans.len() < 2 || base_font_size <= 0.0 {
        return plan;
    }

    // Page baseline extent. Higher y = higher on the page.
    let (mut y_min, mut y_max) = (f32::MAX, f32::MIN);
    for s in spans {
        let y = s.span.bbox.y;
        y_min = y_min.min(y);
        y_max = y_max.max(y);
    }
    let y_range = y_max - y_min;
    if y_range <= 0.0 {
        return plan;
    }
    let bottom_cut = y_min + 0.18 * y_range;

    // --- Candidate definitions from the bottom band. ---
    let mut bottom: Vec<usize> = (0..spans.len())
        .filter(|&i| spans[i].span.bbox.y <= bottom_cut)
        .collect();
    bottom.sort_by(|&a, &b| {
        crate::utils::safe_float_cmp(spans[b].span.bbox.y, spans[a].span.bbox.y).then(
            crate::utils::safe_float_cmp(spans[a].span.bbox.x, spans[b].span.bbox.x),
        )
    });
    // Group bottom-band spans into lines by baseline proximity.
    let mut def_lines: Vec<Vec<usize>> = Vec::new();
    for &i in &bottom {
        let y = spans[i].span.bbox.y;
        if let Some(last) = def_lines.last_mut() {
            let ly = spans[last[0]].span.bbox.y;
            let tol = spans[i].span.font_size.max(1.0) * 0.6;
            if (ly - y).abs() <= tol {
                last.push(i);
                continue;
            }
        }
        def_lines.push(vec![i]);
    }
    // token, member-span indices, definition text.
    let mut cand_defs: Vec<(FootnoteToken, Vec<usize>, String)> = Vec::new();
    for line in &def_lines {
        let mut ordered = line.clone();
        ordered.sort_by(|&a, &b| {
            crate::utils::safe_float_cmp(spans[a].span.bbox.x, spans[b].span.bbox.x)
        });
        // The leading (marker) span must be in a smaller-than-body font.
        if spans[ordered[0]].span.font_size >= base_font_size * 0.92 {
            continue;
        }
        let mut text = String::new();
        for &si in &ordered {
            let t = spans[si].span.text.trim();
            if t.is_empty() {
                continue;
            }
            if !text.is_empty() && !text.ends_with(' ') {
                text.push(' ');
            }
            text.push_str(t);
        }
        if let Some((token, def)) = split_leading_footnote_def(&text) {
            cand_defs.push((token, ordered, def));
        }
    }
    if cand_defs.is_empty() {
        return plan;
    }

    // --- Candidate inline markers in body text. ---
    let mut cand_markers: Vec<(usize, FootnoteToken)> = Vec::new();
    for i in 0..spans.len() {
        let s = spans[i];
        if s.span.bbox.y <= bottom_cut {
            continue; // markers live in body text, not the bottom band
        }
        let token = match parse_footnote_marker_token(&s.span.text) {
            Some(t) => t,
            None => continue,
        };
        let adj = match adjacent_body_span(spans, i) {
            Some(a) => a,
            None => continue,
        };
        let body = spans[adj];
        // Meaningfully smaller font.
        if s.span.font_size >= body.span.font_size * 0.75 {
            continue;
        }
        // Baseline raised relative to the body span (superscript).
        if s.span.bbox.y <= body.span.bbox.y + body.span.font_size * 0.1 {
            continue;
        }
        // Still on roughly the same line (inline, not a separate row).
        if (s.span.bbox.y - body.span.bbox.y).abs() > body.span.font_size * 1.2 {
            continue;
        }
        // A footnote reference attaches to the end of a WORD ("compressible¹").
        // A raised digit that immediately follows another digit (or a lone
        // variable) is a sub/superscript inside an equation, not a footnote —
        // the false positive in math-heavy academic text, where a math
        // subscript coincidentally matches a real footnote number. Suppress a
        // marker whose immediate predecessor on the same line ends in a digit.
        if let Some(prev) = i.checked_sub(1) {
            let p = spans[prev];
            let same_line =
                (p.span.bbox.y - s.span.bbox.y).abs() <= s.span.font_size.max(1.0) * 1.5;
            if same_line {
                if let Some(last) = p.span.text.trim_end().chars().next_back() {
                    if last.is_ascii_digit() {
                        continue;
                    }
                }
            }
        }
        cand_markers.push((i, token));
    }
    if cand_markers.is_empty() {
        return plan;
    }

    // --- Confirm tokens present in BOTH sets, assign labels. ---
    let def_tokens: HashSet<String> = cand_defs.iter().map(|(t, _, _)| t.key()).collect();
    // token key → label name used inside `[^ ]` (digits, or a sequential
    // id for symbols in first-appearance order).
    let mut token_name: HashMap<String, String> = HashMap::new();
    let mut symbol_counter: u32 = 0;
    for (i, token) in &cand_markers {
        let key = token.key();
        if !def_tokens.contains(&key) {
            continue;
        }
        let name = token_name
            .entry(key)
            .or_insert_with(|| match token {
                FootnoteToken::Number(s) => s.clone(),
                FootnoteToken::Symbol(_) => {
                    symbol_counter += 1;
                    symbol_counter.to_string()
                }
            })
            .clone();
        plan.inline_markers.insert(*i, format!("[^{name}]"));
    }
    if plan.inline_markers.is_empty() {
        return plan;
    }

    // --- Emit definitions for confirmed tokens (first line per token). ---
    let mut seen: HashSet<String> = HashSet::new();
    let mut ordered_defs: Vec<(u32, String)> = Vec::new();
    for (token, indices, def) in &cand_defs {
        let key = token.key();
        let name = match token_name.get(&key) {
            Some(n) => n.clone(),
            None => continue,
        };
        if !seen.insert(key) {
            continue;
        }
        for &si in indices {
            plan.definition_spans.insert(si);
        }
        let order = name.parse::<u32>().unwrap_or(u32::MAX);
        ordered_defs.push((order, format!("[^{name}]: {def}")));
    }
    ordered_defs.sort_by_key(|(o, _)| *o);
    plan.definitions = ordered_defs.into_iter().map(|(_, s)| s).collect();
    plan
}
