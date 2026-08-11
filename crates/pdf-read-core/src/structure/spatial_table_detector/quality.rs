use super::*;

/// Validate that an extracted table is not a false positive.
///
/// Rejects:
/// - Tables with too many empty cells (> 60%).
/// - 2-column tables that contain a **continuation-row signature**: any
///   row whose left-hand cell is empty while the right-hand cell is
///   non-empty. Product data sheets draw faint cell backgrounds behind
///   label/value rows, which the spatial detector can cluster into tiny
///   2-column tables; when the right-hand value wraps onto a second line,
///   the continuation row leaves an empty left-hand label cell beside
///   the wrapped value text. This exact shape is a reliable false-positive
///   signal. Sparse 2-column tables with *legitimately* missing right-hand
///   values (e.g. "Fax: ", "N/A" rows) are NOT rejected by this rule.
pub(super) fn is_valid_table(table: &Table) -> bool {
    if table.rows.is_empty() || table.col_count == 0 {
        return false;
    }

    let total_cells = table.rows.len() * table.col_count;
    let empty_cells = table
        .rows
        .iter()
        .flat_map(|r| &r.cells)
        .filter(|c| c.text.trim().is_empty())
        .count();
    let empty_ratio = empty_cells as f32 / total_cells.max(1) as f32;

    if empty_ratio > 0.6 {
        return false;
    }

    // Narrow false-positive signature: a 2-column "table" emitted from
    // label/value rows with faint cell backgrounds, where the right-hand
    // value wraps onto a continuation line. The continuation row has an
    // empty left label cell next to a non-empty right value cell. Reject
    // only this specific shape so legitimate sparse 2-column tables
    // (missing values on the right, blank section headers, etc.) still
    // validate.
    if table.col_count == 2 {
        let has_continuation_row = table.rows.iter().any(|r| {
            r.cells.len() == 2
                && r.cells[0].text.trim().is_empty()
                && !r.cells[1].text.trim().is_empty()
        });
        if has_continuation_row {
            return false;
        }
    }

    true
}

/// Additional gate applied to SPATIAL-only table detection (no explicit
/// lines/rulings): reject "word-per-cell" false positives where a
/// paragraph's visual gaps accidentally align into columns.
///
/// Signature: >=5 columns AND >70% of non-empty cells contain only a
/// single word. Real data tables have multi-word labels, numeric values,
/// or dense content; a paragraph mis-read as a table reads as a sentence
/// when the cells are concatenated.
///
/// This gate is NOT applied when rulings/lines define the table — in
/// that case the author explicitly marked the structure and we trust it
/// even if cells are single-character (census forms, sparse grids).
pub(super) fn passes_spatial_quality_gate(table: &Table) -> bool {
    if table.col_count < 5 {
        return true;
    }
    let non_empty: Vec<&str> = table
        .rows
        .iter()
        .flat_map(|r| &r.cells)
        .map(|c| c.text.trim())
        .filter(|t| !t.is_empty())
        .collect();
    if non_empty.is_empty() {
        return true;
    }
    // A genuine numeric data table (financial / metrics slides) is legitimately
    // almost all single tokens — every cell is a *number* — so the generic
    // single-word prose gate below would wrongly reject it and flatten it into a
    // bold label plus run-on numbers. Bypass the gate
    // ONLY when the table is clearly numeric-DOMINATED (≥50% of non-empty cells
    // are data values). This is deliberately strict: number-heavy prose (an
    // academic page with inline citations/equations whose words happen to align
    // into columns) stays below 50% numeric and is still held to the prose gate,
    // so the bypass does not manufacture false tables.
    let data_values = non_empty.iter().filter(|t| is_data_value(t)).count();
    if data_values * 2 >= non_empty.len() {
        return true;
    }
    // Otherwise: high single-word density is the signature of prose split into
    // one-word columns by aligned inter-word gaps — reject.
    let single_word_count = non_empty
        .iter()
        .filter(|t| t.split_whitespace().count() <= 1)
        .count();
    let ratio = single_word_count as f32 / non_empty.len() as f32;
    ratio <= 0.7
}

/// A numeric / data value token: digits plus the usual numeric punctuation
/// (decimal point, thousands comma, percent, sign, currency). Requires at least
/// one digit so a bare `+` or `$` is not treated as data. Used so numeric-table
/// cells do not read as prose fragments in the spatial quality gate.
pub(super) fn is_data_value(t: &str) -> bool {
    !t.is_empty()
        && t.chars().any(|c| c.is_ascii_digit())
        && t.chars().all(|c| {
            c.is_ascii_digit()
                || matches!(
                    c,
                    '.' | ',' | '%' | '+' | '-' | '\u{2212}' | '$' | '\u{20AC}' | '\u{00A3}'
                )
        })
}

/// Reject a spatial (no-rulings) "table" whose rows are wrapped paragraph
/// lines — a flowing prose page (heading + body paragraph + footer) whose
/// inter-word gaps coincidentally aligned into columns.
///
/// Signature: at least one row, when its non-empty cells are concatenated
/// left-to-right, crosses a SENTENCE boundary mid-row — an alphanumeric
/// character, a sentence terminator, a space, then a new word (e.g. "...to
/// 23,500. Stockout rate..."). Real data-table rows hold values/labels, not
/// running sentences that span a terminator into the next clause, so this
/// almost never fires on genuine tables. Only applied to spatial tables (the
/// caller is the no-rulings path); ruled tables are author-marked and
/// trusted.
///
/// Case is checked via the Unicode general category (`char::is_uppercase` /
/// `is_lowercase`), not ASCII-only, so cased non-Latin scripts (Greek,
/// Cyrillic, Armenian, …) get the same "new sentence starts with a capital"
/// signal Latin does. Scripts with no case distinction at all (Bengali,
/// Devanagari, …) can't use that signal, so their sentence-final danda
/// (`।`, `॥`) is instead treated as a terminator in its own right: a danda
/// followed by a space and another letter mid-row is itself the
/// discriminator, since a genuine data cell doesn't embed a sentence stop
/// followed by more prose in the same row.
pub(super) fn looks_like_prose_paragraph(table: &Table) -> bool {
    const CASELESS_TERMINATORS: [char; 2] = ['।', '॥'];

    for row in &table.rows {
        let joined = row
            .cells
            .iter()
            .map(|c| c.text.trim())
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        let chars: Vec<char> = joined.chars().collect();
        for i in 0..chars.len() {
            // Cased terminator: the exact prior-release strict signature — a
            // lowercase/digit, then `.`/`!`/`?`, then space + capital +
            // lowercase (a genuine new sentence). Kept ASCII with immediate
            // neighbours on purpose: broadening it (any alphanumeric before,
            // skip-spaces, Unicode case) over-rejected genuine data tables
            // whose cells contain abbreviations or capitalised codes ("Fig.
            // 3", "Dr. Smith", "A. Test") as prose.
            if matches!(chars[i], '.' | '!' | '?')
                && i >= 1
                && (chars[i - 1].is_ascii_lowercase() || chars[i - 1].is_ascii_digit())
                && i + 3 < chars.len()
                && chars[i + 1] == ' '
                && chars[i + 2].is_ascii_uppercase()
                && chars[i + 3].is_ascii_lowercase()
            {
                return true;
            }
            // Caseless terminator (Bengali/Devanagari danda ।/॥): scripts with
            // no case distinction can't use the capital-start signal, so a
            // danda followed (allowing a stray positioning gap) by another
            // letter mid-row is itself the tell — a genuine data cell doesn't
            // embed a sentence stop followed by more prose in the same row.
            if CASELESS_TERMINATORS.contains(&chars[i]) {
                let Some(&prev) = chars[..i].iter().rev().find(|c| **c != ' ') else {
                    continue;
                };
                if !prev.is_alphanumeric() {
                    continue;
                }
                let mut after = chars[i + 1..].iter().copied().skip_while(|c| *c == ' ');
                if after.next().is_some_and(|c| c.is_alphabetic()) {
                    return true;
                }
            }
        }
    }

    // Punctuation-independent fallback: a wrapped-prose row whose column
    // break fell mid-clause carries no sentence terminator at all (a title
    // caption, a clause with no `.`/`!`/`?`), so the checks above miss it.
    // Real data-cell values are atomic (numbers, codes, capitalised labels)
    // and essentially never start with a lowercase letter; running-sentence
    // fragments frequently do ("text with", "a smaller", "on"). This mirrors
    // `looks_like_prose_table`'s per-cell signal in document.rs, but applies
    // PER ROW with no minimum cell count — that function's ≥10-cell floor
    // (needed there to avoid over-rejecting small genuine tables under a
    // table-wide ratio) is exactly what lets a small (2–3 row) fabricated
    // table slip through untested. A 2-cell floor plus a majority-of-row
    // requirement keeps ordinary short data rows (a units column, a coded
    // abbreviation) safe: those cells are numeric/capitalised, not lowercase
    // clause fragments, so they rarely cross the 50% bar even at n=2.
    for row in &table.rows {
        let cells: Vec<&str> = row
            .cells
            .iter()
            .map(|c| c.text.trim())
            .filter(|t| !t.is_empty())
            .collect();
        if cells.len() < 2 {
            continue;
        }
        let lower_starts = cells
            .iter()
            .filter(|c| c.chars().next().is_some_and(char::is_lowercase))
            .count();
        if lower_starts as f32 / cells.len() as f32 > 0.5 {
            return true;
        }
    }

    // Vertically-stacked single-character lines: a rotated axis label or
    // figure legend, drawn one glyph per line so its bbox flattens into the
    // page's normal (unrotated) column grid, reads as a cell whose text is
    // mostly 1-character lines joined by `\n` (e.g. "k\nc\no\nc\nE-Box" for a
    // vertically-drawn "clock"-style label). A genuine multi-line data cell
    // wraps at WORD boundaries — its lines hold whole words, not lone
    // letters — so this shape is essentially unique to misread rotated text.
    for row in &table.rows {
        for cell in &row.cells {
            let lines: Vec<&str> = cell
                .text
                .split('\n')
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .collect();
            if lines.len() < 3 {
                continue;
            }
            let single_char_lines = lines.iter().filter(|l| l.chars().count() == 1).count();
            if single_char_lines as f32 / lines.len() as f32 > 0.5 {
                return true;
            }
        }
    }

    false
}

/// Reject a spatial (no-rulings) "table" that is actually CJK prose split into
/// columns by the wrap geometry of an unspaced script. CJK (Chinese, Japanese,
/// Korean Han/kana) writes without inter-word spaces, so wrapped prose lines
/// align into a grid the column detector mistakes for a table. A genuine CJK
/// data cell is short (a label or a number); a prose fragment is a long run of
/// ideographs/kana. Reject when any cell holds a run of `MIN_CJK_RUN` or more
/// consecutive CJK characters. Ruled / author-marked tables never reach here.
pub(super) fn looks_like_cjk_prose(table: &Table) -> bool {
    const MIN_CJK_RUN: usize = 12;
    fn is_cjk(c: char) -> bool {
        matches!(
            c as u32,
            0x3040..=0x30FF      // Hiragana + Katakana
            | 0x3400..=0x4DBF    // CJK Ext A
            | 0x4E00..=0x9FFF    // CJK Unified
            | 0xF900..=0xFAFF    // CJK Compatibility
            | 0xFF66..=0xFF9F    // Halfwidth Katakana
            | 0xAC00..=0xD7AF    // Hangul Syllables
        )
    }
    table.rows.iter().flat_map(|r| &r.cells).any(|c| {
        let mut run = 0usize;
        for ch in c.text.chars() {
            if is_cjk(ch) {
                run += 1;
                if run >= MIN_CJK_RUN {
                    return true;
                }
            } else if !ch.is_whitespace() {
                run = 0;
            }
        }
        false
    })
}

/// Reject a spatial (no-rulings) "table" that is actually a bulleted list whose
/// markers and bodies aligned into columns. A genuine data table never carries
/// a cell that is *only* a bullet glyph; an untagged list rendered into two
/// columns (`• | Ship the API.`) does. Catches the structured-document
/// false-positive where a heading + bulleted list + prose are mis-fused into a
/// grid. Ruled / author-marked tables never reach this path.
pub(super) fn looks_like_bulleted_list(table: &Table) -> bool {
    /// An unambiguous bullet glyph (never a legitimate data value).
    fn is_bullet_glyph(c: char) -> bool {
        matches!(
            c,
            '\u{2022}'
                | '\u{2023}'
                | '\u{2043}'
                | '\u{2219}'
                | '\u{25AA}'
                | '\u{25CF}'
                | '\u{25E6}'
                | '\u{00B7}'
                | '\u{2024}'
        )
    }
    fn is_list_item(t: &str) -> bool {
        let mut chars = t.chars();
        match chars.next() {
            // Leads with a bullet glyph (lone "•" or "• item") → a list item.
            Some(c) if is_bullet_glyph(c) => true,
            // A lone "*" cell is also a marker (but "*5" footnote-data is not).
            Some('*') => chars.next().is_none(),
            _ => false,
        }
    }
    table
        .rows
        .iter()
        .flat_map(|r| &r.cells)
        .filter(|c| !c.text.trim().is_empty())
        .any(|c| is_list_item(c.text.trim()))
}
