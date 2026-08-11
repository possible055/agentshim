use super::*;

/// Unified space decision function - SINGLE SOURCE OF TRUTH for space insertion.
///
/// This function consolidates all space insertion logic into one place per the
/// design principle in the comprehensive plan. It evaluates multiple signals
/// returns a definitive decision about whether to insert a space between spans.
///
/// # Rules (in priority order)
///
/// **Rule 0**: Check if boundary space already exists (from trailing/leading whitespace)
/// - If preceding text ends with space OR following text starts with space, don't insert
/// - Confidence: 1.0 (deterministic)
///
/// **Rule 1**: TJ offset triggered flag
/// - If the TJ processor set the flag due to negative offset > threshold, insert space
/// - This is explicit PDF positioning information
/// - Confidence: 0.95 (highest, explicit signal)
///
/// **Rule 2**: Dual threshold (PDFBox pattern) with document-type adjustment
/// - Calculate both space-width-based and char-width-based thresholds
/// - Adjust thresholds based on document type (Academic/Policy/Mixed)
/// - Use MINIMUM of the two for robustness
/// - If gap exceeds this threshold, insert space
/// - Confidence: 0.8 (geometric measurement)
///
/// **Rule 3**: Character heuristic (CamelCase, number->letter, etc.)
/// - Detect character transitions indicating word boundaries
/// - If heuristic fires, insert space
/// - Confidence: 0.6 (pattern-based)
///
/// **Rule 4**: Conservative threshold (document-type aware)
/// - If gap exceeds conservative threshold (very small), insert space
/// - Catches small intentional gaps that are still word boundaries
/// - Adaptive to document type (Policy uses lower threshold, Academic uses higher)
/// - Confidence: 0.5 (conservative)
///
/// **Default**: No space inserted
///
/// # Document Type Adjustment
///
/// When document_type is provided, thresholds are adjusted:
/// - **Academic** (1.4x multiplier): Higher thresholds for loose spacing
/// - **Policy** (0.6x multiplier): Lower thresholds for tight justified text
/// - **Mixed** (1.0x multiplier): Default/balanced approach
///
/// This matches research findings from LA-PDFText, pdfminer.six, PDFBox, and iText
/// that adaptive thresholds provide better results than fixed values.
///
/// # PDF Spec Reference
///
/// ISO 32000-1:2008, Section 9.4.4 NOTE 6:
/// "The identification of what constitutes a word is unrelated to how the text
/// happens to be grouped into show strings... text strings should be as long as possible."
/// Recover an honest inter-glyph gap for the space-insertion decision.
///
/// Per ISO 32000-1:2008 §9.4.4, the spacing between two glyphs is the
/// text-space displacement between their origins; a word space exists when
/// that displacement reaches the font's space advance. We measure it from
/// the bounding boxes (`raw_gap = next.x − prev.right_edge`).
///
/// When the previous span's font has no explicit `/Widths` array,
/// `FontInfo` substitutes a fixed fallback advance (~0.55 em) that
/// systematically OVER-reports proportional Latin glyphs. That inflates
/// `bbox.width`, pushing `prev.right_edge` past the real glyph end so it can
/// swallow a true word gap and drive `raw_gap` NEGATIVE — glyphs that do not
/// actually overlap appear to. Only in that overlap case do we
/// divide out the fallback inflation (0.55 em ÷ 0.45 em ≈ 1.22) to restore a
/// believable gap.
///
/// Crucially, the correction is applied ONLY when `raw_gap < 0`. When the
/// glyphs do not overlap (`raw_gap ≥ 0`) the layout is already honest
/// must not be second-guessed: inflating a non-overlapping gap manufactures
/// a phantom word space and splits single words that were positioned
/// edge-to-edge — e.g. a CamelCase brand "SalesForce" emitted as
/// "SalesF" + "orce" with `raw_gap == 0` would otherwise be torn into
/// "SalesF orce". (`bbox.width × (1 − 1/1.22)` is the algebraic form of
/// `next.x − (prev.x + width/1.22)` once `raw_gap` is substituted in.)
pub(super) fn corrected_space_gap(
    raw_gap: f32,
    reliable_widths: bool,
    bbox_width: f32,
    text_empty: bool,
) -> f32 {
    if !reliable_widths && raw_gap < 0.0 && bbox_width > 0.0 && !text_empty {
        raw_gap + bbox_width * (1.0 - 1.0 / 1.22)
    } else {
        raw_gap
    }
}

/// detect whether a glyph's mapped text
/// represents an AGL Latin ligature (`/ff` / `/fi` / `/fl` / `/ffi` /
/// `/ffl`). When the upstream space-emission heuristic processes a
/// glyph adjacent to a ligature, the small intra-word kerning that
/// surrounds the ligature glyph can trigger spurious space
/// insertion (producing `di ff cult` for `difficult`). The detection
/// here lets the heuristic suppress space insertion at ligature
/// boundaries.
///
/// Returns true when the text *is* a bare AGL ligature glyph — a
/// single codepoint in the Latin Ligatures block (U+FB00..U+FB06) or
/// the multi-char ASCII fallback ("ff"/"fi"/"fl"/"ffi"/"ffl"). The
/// suppression at the call site targets the pdfTeX-style emission
/// pattern where the ligature is its own cluster between two
/// intra-word fragments (e.g. "di"→"ﬃ"→"cult" or "di"→"ffi"→"cult").
/// A multi-char cluster that merely starts with a ligature
/// (e.g. "ﬂuid" or "ffective") is a full word whose boundary with the
/// previous span is a legitimate space, so we return false in that
/// case.
#[inline]
pub(crate) fn starts_with_agl_ligature(text: &str) -> bool {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    // Bare single-codepoint ligature glyph from the Latin Ligatures
    // block.
    if ('\u{FB00}'..='\u{FB06}').contains(&first) && chars.next().is_none() {
        return true;
    }
    // Multi-character AGL outputs from non-PUA fallbacks — match only
    // when the cluster IS the ligature, never when it just begins
    // with one.
    matches!(text, "ff" | "fi" | "fl" | "ffi" | "ffl")
}

/// detect monospace fonts by name.
/// Monospace fonts emit one show-text op per glyph with one-em
/// advance positioning, which triggers the proportional-font space-
/// emission heuristic to fire inside ordinary tokens. Bumping the
/// threshold for these fonts closes the `function add (a , b )` repro
/// from `code_and_formula.pdf` (issue ). Used by
/// [`should_insert_space`] to switch its `word_margin_ratio` to
/// `1.2` for monospace.
///
/// Names matched case-insensitively. Covers the major monospace
/// families on macOS / Linux / Windows + the pdfTeX-emitted
/// Computer Modern Typewriter (CMTT*) and Latin Modern Mono
/// (LMMono*) families that frequently appear in academic PDFs.
pub(crate) fn is_monospace_font(font_name: &str) -> bool {
    let lower = font_name.to_lowercase();
    const MONO_MARKERS: &[&str] = &[
        "mono",
        "courier",
        "consolas",
        "menlo",
        "fira code",
        "fira mono",
        "source code",
        "inconsolata",
        "cmtt",   // pdfTeX Computer Modern Typewriter
        "lmmono", // Latin Modern Mono (pdfTeX)
        "letter gothic",
        "ocr ", // OCR-A, OCR-B
        "fixedsys",
        "terminal",
    ];
    MONO_MARKERS.iter().any(|m| lower.contains(m))
}

/// True for codepoints in the main emoji / pictographic blocks.
///
/// Used only as a word-spacing hint — ISO 32000-1:2008 §9.10 leaves word
/// segmentation to the reader. Deliberately **excludes** arrows
/// (U+2190–U+21FF) and the math-operator blocks so symbolic/technical text is
/// unaffected; restricted to clearly pictographic ranges plus the VS16 emoji
/// presentation selector.
pub(crate) fn is_pictographic(c: char) -> bool {
    matches!(c as u32,
        0x1F300..=0x1FAFF   // Misc & Supplemental Symbols and Pictographs, Ext-A
        | 0x1F000..=0x1F0FF // Mahjong / Dominoes / Playing cards
        | 0x2600..=0x27BF   // Misc Symbols + Dingbats
        | 0xFE0F) // VS16 emoji presentation selector
}

/// Remove an ASCII space sitting directly between a CJK ideograph/kana and an
/// ASCII digit (either direction). In Chinese and Japanese an embedded number
/// attaches to the surrounding ideographs with no space (e.g. "公元前1000年",
/// "10,000年"); some producers — notably headless-browser print-to-PDF — emit a
/// stray space glyph at that script transition. CJK↔CJK and CJK↔letter spacing
/// is left untouched, so genuine word/term spacing is preserved.
///
/// Hangul is deliberately EXCLUDED: Korean, unlike Chinese/Japanese, is written
/// with inter-word spaces, so a space between a Korean syllable and a number is
/// a real word boundary (e.g. "14 예" = "14 cases", "7 예중") — stripping it
/// corrupts the text. Only the space-less scripts (CJK ideographs + kana) are
/// treated as number-adjacent.
pub(crate) fn strip_cjk_digit_boundary_spaces(text: &str) -> String {
    if !text.contains(' ') {
        return text.to_string();
    }
    let is_cjk = |c: char| {
        matches!(c as u32,
            0x3040..=0x30FF      // Hiragana + Katakana
            | 0x3400..=0x4DBF    // CJK Ext A
            | 0x4E00..=0x9FFF    // CJK Unified
            | 0x20000..=0x2A6DF  // CJK Ext B
            | 0xFF66..=0xFF9F    // Halfwidth Katakana
        )
    };
    // A bracket hugs its content in every script, so a space between a CJK or
    // Hangul character and an adjacent bracket is a layout artifact, not a word
    // break (e.g. Korean "고양이(학명: …)" / "카투스[*]" — the paren and the
    // reference marker sit flush against the syllable). Hangul IS included here,
    // unlike the digit case above: the digit boundary is a real Korean word
    // break, but the bracket boundary never is. Full-width CJK brackets carry
    // their own spacing and are left alone.
    let is_cjk_or_hangul = |c: char| {
        is_cjk(c)
            || matches!(c as u32,
                0xAC00..=0xD7A3   // Hangul syllables
                | 0x1100..=0x11FF // Hangul Jamo
                | 0x3130..=0x318F // Hangul Compatibility Jamo
            )
    };
    let is_hug_bracket = |c: char| matches!(c, '(' | ')' | '[' | ']' | '{' | '}');
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == ' ' && i > 0 && i + 1 < chars.len() {
            let (p, n) = (chars[i - 1], chars[i + 1]);
            if (is_cjk(p) && n.is_ascii_digit()) || (p.is_ascii_digit() && is_cjk(n)) {
                i += 1; // drop the artifact space
                continue;
            }
            if (is_cjk_or_hangul(p) && is_hug_bracket(n))
                || (is_hug_bracket(p) && is_cjk_or_hangul(n))
            {
                i += 1; // drop the artifact space hugging a bracket
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Remove an ASCII space that the geometric word-break heuristic injected inside
/// a prime-notation number, e.g. `0′′.28` → `0′′ .28` or `0′′. 28`.
///
/// Arc-second / arc-minute values attach their decimal fraction to the prime
/// without a break (`0′′.28`, `1′′.47`). A prime glyph's metric advance (w0,
/// ISO 32000-1 §9.4.4) is narrow relative to its inked form, so the gap to the
/// following `.NN` reads as wider than a space and the heuristic splits the
/// token. Two artifact positions are repaired:
///   • prime → `.`   (`′ .` → `′.`)
///   • `.` → digit, when the `.` directly follows a prime (`′. 2` → `′.2`)
///
/// Feet-and-inches like `5′ 6″` are left untouched: the space there sits between
/// a prime and a *digit* (not a `.`), which is a genuine measurement boundary.
pub(crate) fn strip_prime_decimal_boundary_spaces(text: &str) -> String {
    if !text.contains(' ') {
        return text.to_string();
    }
    let is_prime = |c: char| matches!(c, '\u{2032}' | '\u{2033}' | '\u{2034}');
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == ' ' && i > 0 && i + 1 < chars.len() {
            let (p, n) = (chars[i - 1], chars[i + 1]);
            // `′ .28` — space between the prime and the decimal point.
            if is_prime(p) && n == '.' {
                i += 1;
                continue;
            }
            // `′. 28` — space between the prime's decimal point and its digits.
            if p == '.' && n.is_ascii_digit() && i >= 2 && is_prime(chars[i - 2]) {
                i += 1;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

/// True when any drawn glyph run puts ink inside the horizontal gap between
/// `left` and `right`, overlapping their vertical band.
///
/// Used by the decimal-value merge: two pure-digit runs a split-box-sized
/// gap apart merge into one decimal amount ONLY if the gap is empty. A
/// separator glyph occupying the gap — the comma of a subscript index pair
/// (`P_{1,0}`), a list delimiter — proves the runs are distinct tokens, no
/// matter where in the content stream it was drawn. The pair's own boxes
/// bound the gap exactly, so a small epsilon keeps them (and touching
/// neighbours) from counting as intruders.
pub(super) fn decimal_gap_has_ink(ink_boxes: &[Rect], left: &Rect, right: &Rect) -> bool {
    const EPS: f32 = 0.01;
    let gap_start = left.x + left.width;
    let gap_end = right.x;
    if gap_end - gap_start <= 2.0 * EPS {
        return false;
    }
    let band_bottom = left.y.min(right.y);
    let band_top = (left.y + left.height).max(right.y + right.height);
    ink_boxes.iter().any(|b| {
        b.x + b.width > gap_start + EPS
            && b.x < gap_end - EPS
            && b.y < band_top
            && b.y + b.height > band_bottom
    })
}

/// True when a *full intervening glyph* occupies the horizontal gap between
/// `left` and `right` — e.g. a subscript drawn between a variable and the next
/// symbol (`λᵢr…`), which inflates the `λ`→`r` gap though both share a
/// baseline. Distinct from [`decimal_gap_has_ink`]: it requires an ink box to
/// cover a substantial fraction (>= 35%) of the gap width, so a mere
/// descender/ascender edge of an adjacent glyph clipping the gap band does NOT
/// count. Used by the #847 narrow-word-gap rescue to suppress splitting a math
/// sub/superscript from its base while still recovering ordinary prose word
/// gaps (whose gaps are empty of intervening ink).
pub(super) fn gap_has_intervening_glyph(ink_boxes: &[Rect], left: &Rect, right: &Rect) -> bool {
    let gap_start = left.x + left.width;
    let gap_end = right.x;
    let gap_w = gap_end - gap_start;
    if gap_w <= 0.5 {
        return false;
    }
    let band_bottom = left.y.min(right.y);
    let band_top = (left.y + left.height).max(right.y + right.height);
    ink_boxes.iter().any(|b| {
        let overlap = (b.x + b.width).min(gap_end) - b.x.max(gap_start);
        overlap > gap_w * 0.35 && b.y < band_top && b.y + b.height > band_bottom
    })
}

/// Prefixes that form a real hyphenated compound rather than the first half of a word
/// broken across lines. Drawn from the list in [`crate::text::hyphenation`], reduced to
/// the entries that are not also a plausible opening syllable: `re-`, `co-`, and `over-`
/// are omitted because `re-sults`, `co-ding`, and `over-lap` are ordinary line breaks.
///
/// These matter more than their length suggests on academic PDFs, where `pre-training`,
/// `self-attention`, `multi-head`, and `fine-tuning` are high-frequency technical terms
/// whose hyphen carries meaning.
pub(super) const COMPOUND_PREFIXES: [&str; 12] = [
    "self", "non", "anti", "multi", "semi", "cross", "inter", "intra", "counter", "ultra", "pre",
    "fine",
];

/// Whether a line-ending hyphen exists only because one word was split across two lines.
///
/// Removing it rejoins `implementa-` + `tion` into `implementation`, which is what the
/// author wrote; keeping it leaves a hyphen the page never contained as a word character.
///
/// Two guards keep real hyphens. A hyphen is dropped only between two lowercase letters,
/// which excludes capitalised compounds (`Fine-` + `Tuning`), number ranges
/// (`2019-` + `2020`), and headings set in caps; and never after a
/// [`COMPOUND_PREFIXES`] word, which excludes the technical compounds that carry meaning.
///
/// A lowercase compound outside that list which happens to break at its own hyphen —
/// `state-` + `of-the-art` — is still rejoined wrongly. Telling it from a split word
/// needs a lexicon; the failure costs a hyphen rather than a word, and a compound
/// breaking exactly at its hyphen is far rarer in body text than ordinary hyphenation.
pub(crate) fn splits_one_word(preceding: &str, following: &str) -> bool {
    let Some(stem) = preceding.strip_suffix('-') else {
        return false;
    };
    if !stem.chars().last().is_some_and(char::is_lowercase)
        || !following.chars().next().is_some_and(char::is_lowercase)
    {
        return false;
    }
    let word = stem
        .rsplit(|c: char| c.is_whitespace() || c == '-')
        .next()
        .unwrap_or(stem);
    !COMPOUND_PREFIXES.contains(&word)
}
