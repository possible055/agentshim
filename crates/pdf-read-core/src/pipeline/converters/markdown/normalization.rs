use super::*;

pub(super) static RE_URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(https?://[^\s<>\[\]]*[^\s<>\[\].,!?;:])").unwrap());
pub(super) static RE_EMAIL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"([a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,})").unwrap());

/// Detect markdown table separator rows like `|---|---|` or
/// `| :--- | ---: |`. A line qualifies if every `|`-delimited cell is
/// a sequence of `-` (with optional surrounding `:` for alignment) and
/// optional spaces. At least two cells required so single-pipe lines
/// (which are the very pattern we're trying to escape) do not match.
pub(super) fn is_table_separator_line(line: &str) -> bool {
    let trimmed = line.trim();
    if !trimmed.starts_with('|') || !trimmed.ends_with('|') {
        return false;
    }
    let inner = &trimmed[1..trimmed.len() - 1];
    let cells: Vec<&str> = inner.split('|').collect();
    if cells.len() < 2 {
        return false;
    }
    cells.iter().all(|cell| {
        let c = cell.trim();
        !c.is_empty() && c.chars().all(|ch| ch == '-' || ch == ':')
    })
}

/// Issue #10 band-aid. Walk the rendered markdown line by line; for any
/// line that starts with `|` but is *not* part of a markdown table block
/// (defined as the line itself being a separator, or the next line being
/// a separator, or the previous line already classified as in-table),
/// escape the leading `|` as `\|`. Without this, stray header/footer
/// fragments leak into prose and downstream markdown parsers misread
/// them as malformed table rows, fragmenting subsequent text.
pub(super) fn escape_stray_leading_pipes(s: &str) -> String {
    let lines: Vec<&str> = s.split('\n').collect();
    let mut in_table = vec![false; lines.len()];

    // First pass: classify separator lines and the lines immediately
    // above (header) and below (data rows) that are clearly part of
    // the same table block.
    for (i, line) in lines.iter().enumerate() {
        if is_table_separator_line(line) {
            in_table[i] = true;
            if i > 0 && lines[i - 1].trim_start().starts_with('|') {
                in_table[i - 1] = true;
            }
            // Mark contiguous downstream data rows that also start with `|`.
            let mut j = i + 1;
            while j < lines.len() && lines[j].trim_start().starts_with('|') {
                in_table[j] = true;
                j += 1;
            }
        }
    }

    let mut out = String::with_capacity(s.len());
    for (i, line) in lines.iter().enumerate() {
        if !in_table[i] {
            let leading_ws_len = line.len() - line.trim_start().len();
            let trimmed = &line[leading_ws_len..];
            if let Some(rest) = trimmed.strip_prefix('|') {
                out.push_str(&line[..leading_ws_len]);
                out.push_str("\\|");
                out.push_str(rest);
            } else {
                out.push_str(line);
            }
        } else {
            out.push_str(line);
        }
        if i + 1 < lines.len() {
            out.push('\n');
        }
    }
    out
}

/// Heuristic for the 2-fragment wrapped-heading case used by
/// `merge_consecutive_same_level_headings` (issue #4). Returns true
/// when the two heading fragments visually look like ONE heading split
/// across two lines (wrap), as opposed to two distinct same-level
/// sections.
///
/// Generic, script-agnostic signals (no English word lists):
///   1. First fragment does NOT end with a sentence-terminating
///      punctuation (`.`, `?`, `!`, and their CJK/Arabic equivalents
///      `。`, `？`, `！`, `؟`). Sentence-end is the strong split
///      signal across scripts.
///   2. AND one of:
///      a) first ends with continuation punctuation (`,`, `;`, `、`,
///         `；` — comma / semicolon variants), OR
///      b) second fragment opens with a Unicode-lowercase letter
///         (`\p{Ll}`). A wrapped heading's continuation is virtually
///         always lowercase (or non-cased in scripts that lack case)
///         while a distinct following heading typically begins with a
///         capitalized word.
pub(super) fn looks_like_heading_wrap(first: &str, second: &str) -> bool {
    let first_trim = first.trim_end();
    if let Some(last) = first_trim.chars().last() {
        // Sentence terminators (Latin + CJK + Arabic).
        if matches!(last, '.' | '?' | '!' | '。' | '？' | '！' | '\u{061F}') {
            return false;
        }
        // Continuation punctuation (Latin comma/semicolon + CJK + middle dot).
        if matches!(last, ',' | ';' | '、' | '；' | '·') {
            return true;
        }
    }
    // Lowercase opener on the second fragment, Unicode-aware via
    // char.is_lowercase() (matches `\p{Ll}`).
    let second_first = second.trim_start().chars().next();
    if let Some(c) = second_first {
        if c.is_lowercase() {
            return true;
        }
    }
    false
}

/// Issue #2 fix. Drop consecutive duplicate paragraphs from the final
/// markdown. Duplicates surface in the reporter's corpus when the
/// extractor emits the same content twice (once via the structure
/// pipeline, once via the plaintext fallback). Exact-match only; we
/// will not touch near-duplicates because legitimate prose can repeat
/// a short phrase.
// RETIRED from the active pipeline (see render_spans). Removes legit
// repeated content (distinct form widgets with identical labels,
// repeated headings). Kept for reference + unit-test documentation.
#[allow(dead_code)]
pub(super) fn dedup_consecutive_paragraphs(s: &str) -> String {
    let paras: Vec<&str> = s.split("\n\n").collect();
    let mut out: Vec<&str> = Vec::with_capacity(paras.len());
    let mut prev_norm: Option<String> = None;
    for p in paras {
        let norm: String = p
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if norm.is_empty() {
            out.push(p);
            prev_norm = None;
            continue;
        }
        if prev_norm.as_deref() == Some(norm.as_str()) {
            // Skip — identical to the immediately-previous content paragraph.
            continue;
        }
        prev_norm = Some(norm);
        out.push(p);
    }
    out.join("\n\n")
}

/// Issue #5 fix. Some spatial-grouping artifacts produce header rows
/// where every cell carries the same identifier (e.g. `| Q1'25 |
/// Q1'25 | Q1'25 | Q1'25 |`). Detect such all-identical header rows
/// (marker: the row's next line IS a markdown separator `|---|...|`)
/// and dedup so only the first cell carries the value. Conservative:
/// only fires when ALL non-empty cells are byte-identical AND there
/// are >= 3 cells (single duplicates are too ambiguous to touch).
// RETIRED from the active pipeline (see render_spans). Blanking
// "duplicate" header cells assumes the duplication is an artifact.
// Kept for reference + unit-test documentation.
#[allow(dead_code)]
pub(super) fn dedup_identical_header_cells(s: &str) -> String {
    let lines: Vec<&str> = s.split('\n').collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let next_is_sep = i + 1 < lines.len() && is_table_separator_line(lines[i + 1]);
        let trimmed = line.trim();
        let looks_like_header = trimmed.starts_with('|') && trimmed.ends_with('|');
        if !next_is_sep || !looks_like_header {
            out.push(line.to_string());
            i += 1;
            continue;
        }
        let inner = &trimmed[1..trimmed.len() - 1];
        let cells: Vec<&str> = inner.split('|').collect();
        let non_empty: Vec<&str> = cells
            .iter()
            .map(|c| c.trim())
            .filter(|c| !c.is_empty())
            .collect();
        if non_empty.len() < 3 {
            out.push(line.to_string());
            i += 1;
            continue;
        }
        let first = non_empty[0];
        let all_same = non_empty.iter().all(|c| *c == first);
        if !all_same {
            out.push(line.to_string());
            i += 1;
            continue;
        }
        // Rewrite: keep first cell, blank the rest. Preserve cell count.
        let mut new_cells: Vec<String> = Vec::with_capacity(cells.len());
        let mut wrote_first = false;
        for cell in &cells {
            if cell.trim().is_empty() {
                new_cells.push(String::new());
            } else if !wrote_first {
                new_cells.push(format!(" {} ", cell.trim()));
                wrote_first = true;
            } else {
                new_cells.push(String::from(" "));
            }
        }
        out.push(format!("|{}|", new_cells.join("|")));
        i += 1;
    }
    out.join("\n")
}

/// Issue #1 + #4 fix. Merge runs of consecutive same-level markdown
/// headings into a single heading when the run is unambiguously ONE
/// logical heading. See `looks_like_heading_wrap` for the 2-fragment
/// wrapped-heading rule; otherwise require 3+ fragments each <= 2
/// words (canonical PowerPoint word-per-heading pattern).
/// Is `line` a Markdown list-item marker line (`- `, `* `, `+ `, or an ordered
/// `N.`/`N)` marker)? Used to tighten lists.
pub(super) fn is_md_list_item_line(line: &str) -> bool {
    let t = line.trim_start();
    if let Some(rest) = t
        .strip_prefix("- ")
        .or_else(|| t.strip_prefix("* "))
        .or_else(|| t.strip_prefix("+ "))
    {
        return !rest.trim().is_empty() || rest.is_empty();
    }
    // Ordered marker: leading ASCII digits then '.' or ')' then a space.
    let digits = t.bytes().take_while(|b| b.is_ascii_digit()).count();
    digits > 0
        && matches!(t.as_bytes().get(digits), Some(b'.') | Some(b')'))
        && t.as_bytes().get(digits + 1) == Some(&b' ')
}

/// Collapse blank lines that sit between two consecutive list-item marker lines
/// so the list renders tight (CommonMark §5.3) rather than loose. The span
/// flush in [`MarkdownOutputConverter::convert`] always appends a blank line
/// after each item, which would otherwise wrap every item in its own `<p>` and
/// fragment the document's list structure. Blank lines that separate a list
/// item from non-list content (the end of the list) are preserved.
pub(super) fn tighten_list_items(s: &str) -> String {
    let lines: Vec<&str> = s.split('\n').collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        out.push(lines[i].to_string());
        if is_md_list_item_line(lines[i]) {
            // Look past blank lines to the next content line.
            let mut j = i + 1;
            while j < lines.len() && lines[j].trim().is_empty() {
                j += 1;
            }
            // Only collapse when the next content line is also a list item AND
            // at least one blank line separated them (so we actually tighten).
            if j > i + 1 && j < lines.len() && is_md_list_item_line(lines[j]) {
                i = j;
                continue;
            }
        }
        i += 1;
    }
    out.join("\n")
}

pub(super) fn merge_consecutive_same_level_headings(s: &str) -> String {
    let lines: Vec<&str> = s.split('\n').collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();
        // Capture leading `#`s, require space after.
        let level = trimmed.bytes().take_while(|&b| b == b'#').count();
        let is_heading =
            (1..=6).contains(&level) && trimmed.as_bytes().get(level).copied() == Some(b' ');
        if !is_heading {
            out.push(line.to_string());
            i += 1;
            continue;
        }

        // Accumulate consecutive same-level headings separated only by
        // blank lines. No word-count gate here — policy decision is
        // made AFTER collection so the wrapped-2-fragment case (which
        // tolerates longer fragments) is reachable.
        let mut texts: Vec<String> = vec![trimmed[level + 1..].trim().to_string()];
        let mut j = i + 1;
        loop {
            // Skip blank lines.
            while j < lines.len() && lines[j].trim().is_empty() {
                j += 1;
            }
            if j >= lines.len() {
                break;
            }
            let next_trim = lines[j].trim_start();
            let next_level = next_trim.bytes().take_while(|&b| b == b'#').count();
            let next_is_heading =
                next_level == level && next_trim.as_bytes().get(next_level).copied() == Some(b' ');
            if !next_is_heading {
                break;
            }
            let next_text = next_trim[next_level + 1..].trim().to_string();
            // Hard guard: refuse to even ATTEMPT merge if any single
            // fragment is implausibly long for a heading (> 15 words).
            // That cap is high enough that no real wrapped heading
            // exceeds it, while still preventing pathological fusion.
            if next_text.split_whitespace().count() > 15 {
                break;
            }
            texts.push(next_text);
            j += 1;
        }

        // Two policies that both prove the run is one logical heading:
        //   A) 3+ fragments AND each <= 2 words — canonical PowerPoint
        //      word-per-heading pattern.
        //   B) Exactly 2 fragments AND the FIRST ends with a
        //      continuation-strength punctuation (`,` or `;`) or no
        //      sentence-terminator (`.`, `?`, `!`, `:`). The second
        //      fragment must visually look like a continuation: start
        //      lowercase or with a connector word ("and"/"or"/"the"/
        //      "with"/"of"/...). This matches the reporter's wrapped-
        //      heading shape `## Despite seasonal slowdown,` +
        //      `## warehouse operations maintained...` while still
        //      keeping `# First Heading` / `# Second Heading` apart
        //      (no trailing comma, second word "Second" is capitalized
        //      and not a connector).
        let three_plus_short =
            texts.len() >= 3 && texts.iter().all(|t| t.split_whitespace().count() <= 2);
        let wrapped_two = texts.len() == 2 && looks_like_heading_wrap(&texts[0], &texts[1]);
        if three_plus_short || wrapped_two {
            let merged = texts.join(" ");
            let hashes = "#".repeat(level);
            out.push(format!("{} {}", hashes, merged));
            i = j;
        } else {
            out.push(line.to_string());
            i += 1;
        }
    }
    out.join("\n")
}

/// Issue #9 — DELIBERATELY NOT a post-process filter. Initial
/// implementation regex-matched "Page N" / "N of M" / "— 12 —" at
/// the markdown stage and dropped those lines from the output. That
/// was wrong: it discards legitimate text content. If a PDF actually
/// has "Page 1" in its content stream the correct behavior is to
/// extract it, not silently delete it.
///
/// The proper fix lives upstream and follows the PDF spec
/// (ISO 32000-1:2008 §14.8.2.2 "Artifacts"). Pagination, headers,
/// and footers are supposed to be marked as `/Artifact` marked-
/// content elements; extraction can/should skip artifacts when
/// producing the document's logical text stream. For untagged PDFs
/// without artifact metadata, geometric header/footer detection at
/// extraction time (consistent y-position across pages, repeated
/// content) is the correct heuristic — not a regex that pattern-
/// matches the rendered prose.
///
/// The function is retained as a no-op stub for backward source
/// compatibility (the post-process pipeline below no longer invokes
/// it). Future work: implement the upstream artifact-skip path.
#[allow(dead_code)]
pub(super) fn filter_page_number_lines(s: &str) -> String {
    s.to_string()
}

/// Issue #13 — DELIBERATELY NOT a post-process replacement. The
/// reporter's examples (`•` → `❍`, unexpected `ī`, `Ƅ`, `ώ`) all
/// trace back to font-encoding / ToUnicode CMap misses in the
/// extractor (PARSER_WARNINGS report, 25,350 occurrences of
/// "ToUnicode CMap MISS"). Pattern-replacing codepoints at the
/// markdown layer would MODIFY the document's actual text — if a
/// PDF really uses `❍` deliberately, dropping it to `•` is content
/// corruption, not a fix.
///
/// The correct fix is upstream and follows PDF §9.10 (Extraction of
/// text content): when a Type0 font has no `/ToUnicode` CMap and no
/// recognizable Encoding, fall back to the `/CIDSystemInfo` or
/// glyph-name heuristics rather than emitting garbage codepoints.
/// The bullet symptom disappears for free once the CMap fallback
/// path is robust.
///
/// Function retained as a no-op for backward source compatibility.
#[allow(dead_code)]
pub(super) fn normalize_bullet_glyphs(s: &str) -> String {
    s.to_string()
}

/// Issues #3 / #6 / partial #11 band-aid. Detect "degenerate" markdown
/// table blocks produced by the spatial-table heuristic firing on
/// multi-column prose, and replace them with a single flowing paragraph.
///
/// A table block is considered degenerate when:
///   - >= 5 columns (typical multi-column prose run width),
///   - >= 2 data rows after the header/separator,
///   - >= 60% of non-empty cells contain a single word.
///
/// Such blocks are almost never legitimate data tables — real tables in
/// the test corpus average 2-4 words per cell. The replacement is a
/// best-effort: concatenate every non-empty cell with a single space, in
/// row-major order.
// RETIRED from the active pipeline (see render_spans). Flattened a
// real country-data table in the 70-PDF regression sweep. A
// markdown-layer heuristic cannot reliably distinguish a spurious
// prose "table" from a real sparse one. Kept for reference +
// unit-test documentation.
#[allow(dead_code)]
pub(super) fn simplify_degenerate_tables(s: &str) -> String {
    let lines: Vec<&str> = s.split('\n').collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        // Detect a candidate table: header row + separator + at least one data row.
        let header = lines[i];
        if !header.trim_start().starts_with('|')
            || i + 1 >= lines.len()
            || !is_table_separator_line(lines[i + 1])
        {
            out.push(header.to_string());
            i += 1;
            continue;
        }

        // Collect the full table block.
        let mut block_end = i + 2;
        while block_end < lines.len() && lines[block_end].trim_start().starts_with('|') {
            block_end += 1;
        }
        let block = &lines[i..block_end];

        // Split each row's cells (drop the outer empty cells from the
        // leading/trailing pipes).
        let parse_row = |row: &str| -> Vec<String> {
            row.trim()
                .trim_start_matches('|')
                .trim_end_matches('|')
                .split('|')
                .map(|c| c.trim().to_string())
                .collect()
        };

        let header_cells = parse_row(header);
        let data_rows: Vec<Vec<String>> = block.iter().skip(2).map(|r| parse_row(r)).collect();

        let cols = header_cells.len();
        let data_row_count = data_rows.len();

        if cols < 5 || data_row_count < 2 {
            out.extend(block.iter().map(|l| l.to_string()));
            i = block_end;
            continue;
        }

        // Compute single-word-cell ratio among non-empty cells.
        let mut non_empty = 0usize;
        let mut single_word = 0usize;
        for cell in header_cells.iter().chain(data_rows.iter().flatten()) {
            if cell.is_empty() {
                continue;
            }
            non_empty += 1;
            if cell.split_whitespace().count() == 1 {
                single_word += 1;
            }
        }
        if non_empty == 0 {
            // Pure empty block — drop entirely.
            i = block_end;
            continue;
        }
        let single_ratio = single_word as f32 / non_empty as f32;

        if single_ratio < 0.6 {
            out.extend(block.iter().map(|l| l.to_string()));
            i = block_end;
            continue;
        }

        // Degenerate: flatten to a single paragraph.
        let mut words: Vec<String> = Vec::new();
        for cell in header_cells.iter().chain(data_rows.iter().flatten()) {
            if !cell.is_empty() {
                words.push(cell.clone());
            }
        }
        out.push(words.join(" "));
        i = block_end;
    }
    out.join("\n")
}

/// Issue #11 (partial) band-aid. Detect runs of 2+ consecutive numeric-only
/// H1/H2 headings (e.g. `# 23,500`, `# 99.2%`, `# 87%`, `# 4.2 days`)
/// produced when a KPI dashboard's large numbers were spatially read as
/// stand-alone headings. Convert the run into a bulleted list so the
/// values render as data instead of as section titles. Conservative:
/// every heading in the run must match the numeric pattern; if any one
/// fails, the run is left alone.
pub(super) fn collapse_numeric_heading_runs(s: &str) -> String {
    // Matches a heading line whose body is a short numeric/percentage/
    // currency/duration value. Allowed: digits, comma/period/colon/dash/
    // slash, `%`, `$`, `£`, `€`, optional letters for "K"/"M"/"B"/"days"/
    // "hrs"/"min"/"sec". Capped length keeps real numeric headings
    // (e.g. "# 2024 Annual Report") from matching by accident.
    static RE_NUMERIC_HEADING: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^(#{1,2})\s+([\$£€]?\d[\d,.:\-/]*\s*(?:%|K|M|B|days|day|hrs|hr|min|sec)?)\s*$")
            .unwrap()
    });
    let lines: Vec<&str> = s.split('\n').collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        // Skip blank lines normally.
        if !RE_NUMERIC_HEADING.is_match(lines[i]) {
            out.push(lines[i].to_string());
            i += 1;
            continue;
        }
        // Found one — look ahead for more numeric headings of the same
        // level, allowing blank-line separators.
        let level = lines[i]
            .trim_start()
            .bytes()
            .take_while(|&b| b == b'#')
            .count();
        let mut values: Vec<String> = Vec::new();
        let mut last_match_idx = i;
        let mut j = i;
        while j < lines.len() {
            if lines[j].trim().is_empty() {
                j += 1;
                continue;
            }
            let trim = lines[j].trim_start();
            let l = trim.bytes().take_while(|&b| b == b'#').count();
            if l != level {
                break;
            }
            if let Some(caps) = RE_NUMERIC_HEADING.captures(lines[j]) {
                let v = caps
                    .get(2)
                    .map(|m| m.as_str().trim().to_string())
                    .unwrap_or_default();
                if v.chars().count() > 20 {
                    break;
                }
                values.push(v);
                last_match_idx = j;
                j += 1;
            } else {
                break;
            }
        }
        if values.len() < 2 {
            out.push(lines[i].to_string());
            i += 1;
            continue;
        }
        // Emit as a bulleted list.
        for v in &values {
            out.push(format!("- {}", v));
        }
        out.push(String::new()); // trailing blank line
        i = last_match_idx + 1;
    }
    out.join("\n")
}

/// Issue #12 (narrow) band-aid. Within a single bold block `**...**`,
/// detect the CamelCase fragmentation pattern produced when a word
/// rendered with mixed fonts (e.g. bold first letter, regular rest) is
/// emitted as space-separated fragments inside one bold span. The
/// canonical example from the reporter's corpus is `**S alesF orce**`
/// (intended: `**SalesForce**`).
///
/// Match criteria: a single uppercase ASCII letter followed by a space,
/// then a lowercase chunk that itself contains a later uppercase letter
/// (the CamelCase indicator), then a space and another lowercase chunk.
/// All three pieces must live inside the same `**...**` pair. Replacing
/// `**A bcD efg**` with `**AbcDefg**`.
///
/// Conservative on purpose: matching mid-prose "I am Bob" or "USB Type C"
/// would corrupt legitimate text, so the regex requires the CamelCase
/// signal to be unambiguous (lowercase+uppercase within a single inner
/// fragment).
pub(super) fn coalesce_camelcase_bold_fragments(s: &str) -> String {
    // Unicode-aware (script-agnostic): `\p{Lu}` matches any
    // uppercase letter in Unicode, `\p{Ll}` matches any lowercase
    // letter. The CamelCase signal — a lowercase-letter run
    // containing a later uppercase letter inside one fragment — is
    // unambiguous across Latin, Cyrillic, Greek, Armenian, Coptic,
    // and other cased scripts. Non-cased scripts (CJK, Arabic,
    // Hebrew) lack CamelCase entirely so the pattern can never
    // match — that's correct behavior.
    //
    // Pass 1 — inline form: `**A bcD ef**` (closing `**` after the
    // lowercase tail). Three fragments inside one bold pair.
    static RE_CAMELCASE_BOLD_INLINE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\*\*(\p{Lu})\s+(\p{Ll}+\p{Lu}\p{Ll}*)\s+(\p{Ll}+)\*\*").unwrap()
    });
    // Pass 2 — bound form: `**A bcD** ef` (closing `**` mid-CamelCase,
    // lowercase tail outside the bold). Two fragments inside the bold
    // pair, tail immediately (or after one optional space) after.
    static RE_CAMELCASE_BOLD_BOUND: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\*\*(\p{Lu})\s+(\p{Ll}+\p{Lu}\p{Ll}*)\*\*\s*(\p{Ll}+)").unwrap()
    });
    let pass1 = RE_CAMELCASE_BOLD_INLINE
        .replace_all(s, |caps: &regex::Captures| {
            format!("**{}{}{}**", &caps[1], &caps[2], &caps[3])
        })
        .to_string();
    RE_CAMELCASE_BOLD_BOUND
        .replace_all(&pass1, |caps: &regex::Captures| {
            format!("**{}{}{}**", &caps[1], &caps[2], &caps[3])
        })
        .to_string()
}
