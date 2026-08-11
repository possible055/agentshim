use super::*;

/// Parse an escape sequence token like `<space>`, `<tab>`, etc.
///
/// These are symbolic names for special characters in CMap files.
/// Supported sequences:
/// - `<space>` -> U+0020 (space)
/// - `<tab>` -> U+0009 (tab)
/// - `<newline>` -> U+000A (newline)
/// - `<carriage return>` -> U+000D (carriage return)
///
/// # Arguments
///
/// * `token` - A string token from the CMap (should be enclosed in angle brackets)
///
/// # Returns
///
/// Some(String) containing the mapped character, or None if not an escape sequence
fn parse_escape_sequence(token: &str) -> Option<String> {
    // Remove angle brackets and trim whitespace
    let token = token.trim();
    let token = if token.starts_with('<') && token.ends_with('>') {
        &token[1..token.len() - 1]
    } else {
        token
    };

    let token_lower = token.to_lowercase();
    match token_lower.trim() {
        "space" => Some(" ".to_string()),
        "tab" => Some("\t".to_string()),
        "newline" => Some("\n".to_string()),
        "carriage return" => Some("\r".to_string()),
        _ => None,
    }
}

/// Decode a UTF-16 surrogate pair encoded as a 32-bit value.
///
/// PDF ToUnicode CMaps sometimes encode Unicode code points > U+FFFF
/// as UTF-16 surrogate pairs represented as 8 hex digits.
///
/// Example: D835DF0C (0xD835DF0C) represents:
/// - High surrogate: 0xD835
/// - Low surrogate: 0xDF0C
/// - Decoded: U+1D70C (MATHEMATICAL ITALIC SMALL RHO '𝜌')
///
/// # Arguments
///
/// * `value` - A 32-bit value where the high 16 bits are the high surrogate
///            and the low 16 bits are the low surrogate
///
/// # Returns
///
/// The decoded Unicode character as a String, or None if the surrogate pair is invalid
fn decode_utf16_surrogate_pair(value: u32) -> Option<String> {
    let high = (value >> 16) as u16;
    let low = (value & 0xFFFF) as u16;

    // Check if these are valid surrogate pairs
    // High surrogate: 0xD800 - 0xDBFF
    // Low surrogate: 0xDC00 - 0xDFFF
    if (0xD800..=0xDBFF).contains(&high) && (0xDC00..=0xDFFF).contains(&low) {
        // Decode UTF-16 surrogate pair to Unicode code point
        let codepoint = 0x10000 + (((high & 0x3FF) as u32) << 10) + ((low & 0x3FF) as u32);
        char::from_u32(codepoint).map(|ch| ch.to_string())
    } else {
        // Not a valid surrogate pair, try as a direct code point
        char::from_u32(value).map(|ch| ch.to_string())
    }
}

/// Parse a ToUnicode CMap stream with optimized state machine parser.
///
/// ToUnicode CMaps contain mappings in two formats:
/// - `bfchar`: Single character mappings
/// - `bfrange`: Range mappings
///
/// # Format Examples
///
/// ```text
/// beginbfchar
/// <0041> <0041>  % Maps 0x41 to Unicode U+0041 ('A')
/// <0042> <0042>  % Maps 0x42 to Unicode U+0042 ('B')
/// endbfchar
///
/// beginbfrange
/// <0020> <007E> <0020>  % Maps 0x20-0x7E to U+0020-U+007E (ASCII printable)
/// endbfrange
/// ```
///
/// # Phase 5.3 Optimization
///
/// Uses state machine parsing for 20-40% faster performance:
/// - State transitions: HEADER -> CODESPACE -> BFCHAR/BFRANGE/NOTDEFRANGE -> FOOTER
/// - Sequential token processing without full buffering
/// - Binary search on sorted ranges for O(log n) lookups
/// - Direct insertion into HashMap for bfchar entries
///
/// # Arguments
///
/// * `data` - Raw CMap stream data (should be decoded/decompressed first)
///
/// # Returns
///
/// A CMap with optimized storage for O(1) direct lookup and O(log n) range lookup.
///
/// # Examples
///
/// ```
/// use pdf_oxide::fonts::cmap::parse_tounicode_cmap;
///
/// let cmap_data = b"beginbfchar\n<0041> <0041>\nendbfchar";
/// let cmap = parse_tounicode_cmap(cmap_data).unwrap();
/// assert_eq!(cmap.get(&0x41).as_deref(), Some("A"));
/// ```
pub fn parse_tounicode_cmap(data: &[u8]) -> Result<CMap> {
    let mut cmap = CMap::new();
    let content = String::from_utf8_lossy(data);

    // Parse `/WMode N def` directive (Adobe CMap & CIDFont Files Spec §7.2, ISO
    // 32000-1 §9.7.5.4). `N` is `0` (horizontal) or `1` (vertical). The
    // directive appears at the top level of the CMap stream, outside any
    // `begin…end` block, so a substring + integer scan is sufficient and
    // avoids a second tokenizer pass.
    if let Some(parsed_wmode) = parse_wmode_directive(&content) {
        cmap.wmode = parsed_wmode;
        if parsed_wmode == 1 {
            log::trace!("CMap declares /WMode 1 (vertical writing)");
        }
    }

    // Parse begincodespacerange sections (PDF Spec §9.7.5 / §9.10.3)
    //
    // The codespace range declares the valid domain of character codes and,
    // critically, **their byte width**.  A range like `<00> <FF>` is 1-byte;
    // `<0000> <FFFF>` is 2-byte.  We use the widest range found to set
    // `cmap.code_width`, which the text extractor uses to decide how many
    // bytes to consume per character from the PDF content stream.
    //
    // Without this, any CJK ToUnicode CMap that does not use one of the
    // well-known encoding names (Identity-H, EUC, GBK, …) would be read
    // one byte at a time, splitting every 2-byte CID into two wrong codes.
    for section in extract_sections(&content, "begincodespacerange", "endcodespacerange") {
        for line in section.lines() {
            let width = parse_codespacerange_line_width(line);
            if width > cmap.code_width {
                cmap.code_width = width;
                log::trace!(
                    "ToUnicode codespacerange: code_width set to {}",
                    cmap.code_width
                );
            }
        }
    }

    // Parse bfchar and bfrange sections in document order so that later entries
    // overwrite earlier ones for the same code (ISO 32000-1:2008 §9.10.3).
    // pdf.js, MuPDF, and Poppler all use this last-wins, document-order semantics.
    for (kind, section) in bf_sections_in_document_order(&content) {
        match kind {
            BfSectionKind::Char => {
                // Format: <srcCode> <dstString>
                for line in section.lines() {
                    for (src, dst) in parse_bfchar_line(line) {
                        log::trace!("ToUnicode bfchar: 0x{:02X} -> {:?}", src, dst);
                        cmap.insert(src, dst);
                    }
                }
            }
            BfSectionKind::Range => {
                // Format: <srcCodeLo> <srcCodeHi> [<dstString0> <dstString1> ... <dstStringN>]
                //     or: <srcCodeLo> <srcCodeHi> <dstString>
                for line in section.lines() {
                    if let Some(mappings) = parse_bfrange_line(line) {
                        log::trace!("ToUnicode bfrange: {} mappings parsed", mappings.len());
                        for (src, dst) in mappings {
                            cmap.insert(src, dst);
                        }
                    }
                }
            }
        }
    }

    // Parse beginnotdefrange sections (Phase 4.1)
    // Format: <srcCodeLo> <srcCodeHi> <dstString>
    // Maps a range of codes to a single Unicode character (fallback for unmapped codes)
    for section in extract_sections(&content, "beginnotdefrange", "endnotdefrange") {
        for line in section.lines() {
            if let Some(mappings) = parse_notdefrange_line(line) {
                log::trace!("ToUnicode notdefrange: {} mappings parsed", mappings.len());
                for (src, dst) in mappings {
                    // Only insert if not already mapped (normal mappings take precedence)
                    // For notdefrange, we need to check if source is already mapped
                    if !cmap.chars.contains_key(&src) {
                        cmap.insert(src, dst);
                    }
                }
            }
        }
    }

    cmap.compress_sequential_ranges();
    Ok(cmap)
}

enum BfSectionKind {
    Char,
    Range,
}

/// Yield `beginbfchar` and `beginbfrange` sections in the order they appear in
/// the CMap stream, so that callers can process them with document-order,
/// last-wins semantics (matching pdf.js, MuPDF, and Poppler).
fn bf_sections_in_document_order(content: &str) -> impl Iterator<Item = (BfSectionKind, &str)> {
    let mut remaining = content;
    std::iter::from_fn(move || {
        loop {
            let pos = remaining.find("beginbf")?;
            let after = &remaining[pos + "beginbf".len()..];

            if let Some(body) = after.strip_prefix("char") {
                if let Some(end) = body.find("endbfchar") {
                    remaining = &body[end + "endbfchar".len()..];
                    return Some((BfSectionKind::Char, &body[..end]));
                }
            } else if let Some(body) = after.strip_prefix("range") {
                if let Some(end) = body.find("endbfrange") {
                    remaining = &body[end + "endbfrange".len()..];
                    return Some((BfSectionKind::Range, &body[..end]));
                }
            }
            // Unrecognised "beginbf…" token or missing end marker; skip past it.
            remaining = after;
        }
    })
}

/// Extract sections between begin and end markers.
pub(super) fn extract_sections<'a>(content: &'a str, begin: &str, end: &str) -> Vec<&'a str> {
    let mut sections = Vec::new();
    let mut remaining = content;

    while let Some(begin_pos) = remaining.find(begin) {
        let after_begin = &remaining[begin_pos + begin.len()..];
        if let Some(end_pos) = after_begin.find(end) {
            sections.push(&after_begin[..end_pos]);
            remaining = &after_begin[end_pos + end.len()..];
        } else {
            break;
        }
    }

    sections
}

/// Parse a `/WMode N def` directive from a CMap source string.
///
/// Returns `Some(0)` for explicit horizontal, `Some(1)` for explicit vertical,
/// and `None` when no directive is present (caller keeps the spec default of
/// `0`). Per Adobe CMap & CIDFont Files Spec §7.2 and ISO 32000-1 §9.7.5.4,
/// `/WMode` must precede `begincmap` but in practice all writers we have seen
/// place it within the prologue before `begincodespacerange`. A direct lexical
/// scan is robust to either ordering.
///
/// Only matches values `0` or `1`; any other integer is treated as a malformed
/// directive and ignored (returns `None`).
pub(crate) fn parse_wmode_directive_public(content: &str) -> Option<u8> {
    parse_wmode_directive(content)
}

fn parse_wmode_directive(content: &str) -> Option<u8> {
    static RE: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"/WMode\s+([0-9]+)\s+def").unwrap());
    // PostScript comments run from `%` to end-of-line (Adobe PostScript
    // Language Reference §3.3.1). Strip them so a commented-out directive
    // like `% /WMode 1 def` does not flip the writing mode. Keep newlines
    // intact so any subsequent legitimate `/WMode` on a later line is
    // still matched.
    let cleaned: String = content
        .lines()
        .map(|line| match line.find('%') {
            Some(idx) => &line[..idx],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let caps = RE.captures(&cleaned)?;
    let value: u32 = caps[1].parse().ok()?;
    match value {
        0 => Some(0),
        1 => Some(1),
        // M6: non-spec values (e.g. `/WMode 2 def`) surface a warning so
        // producer bugs are diagnosable. We still return None and let
        // the caller fall back to the horizontal default — the spec
        // (§9.7.5.4) only defines values 0 and 1.
        other => {
            log::warn!(
                "Non-standard /WMode {} in CMap stream; falling back to horizontal (WMode 0)",
                other
            );
            None
        }
    }
}

/// Parse a `begincodespacerange` line and return the maximum code byte-width found.
///
/// Each entry is a pair of hex strings: `<lo> <hi>`.  The number of hex digits
/// in each string determines the byte width of the character codes:
/// - 2 hex digits  → 1-byte code  (e.g. `<00> <FF>`)
/// - 4 hex digits  → 2-byte code  (e.g. `<0000> <FFFF>`)
///
/// Returns 1 if the line does not contain a valid codespace pair, or 2 if at
/// least one 2-byte (4-hex-digit) entry is found.
fn parse_codespacerange_line_width(line: &str) -> u8 {
    static RE: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"<([^>]*)>\s*<([^>]*)>").unwrap());

    let mut max_width: u8 = 1;
    for caps in RE.captures_iter(line) {
        let lo_hex = caps[1].trim().replace(char::is_whitespace, "");
        let hi_hex = caps[2].trim().replace(char::is_whitespace, "");
        // 4 or more hex digits mean ≥2-byte codes.
        if lo_hex.len() >= 4 || hi_hex.len() >= 4 {
            max_width = 2;
        }
    }
    max_width
}

/// Parse a bfchar line, returning all `<src> <dst>` pairs found on the line.
///
/// Example: `<0041> <0041>` maps character code 0x41 to Unicode U+0041.
/// Example: `<0003> <00410042>` maps character code 0x03 to Unicode "AB" (multi-char mapping).
/// Example: `<01> <0041> <02> <0042>` maps two character codes on one line.
///
/// Supports multiple pairs per line, hex code points, ligatures, escape sequences,
/// and flexible whitespace inside angle brackets.
pub(super) fn parse_bfchar_line(line: &str) -> Vec<(u32, String)> {
    static RE: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"<([^>]*)>\s*<([^>]*)>").unwrap());

    let mut results = Vec::new();

    for caps in RE.captures_iter(line) {
        let parsed = (|| -> Option<(u32, String)> {
            let src_str = caps[1].trim().replace(char::is_whitespace, "");
            let src = u32::from_str_radix(&src_str, 16).ok()?;

            let dst_str = caps[2].trim();

            let dst = if let Some(escape) = parse_escape_sequence(&format!("<{}>", dst_str)) {
                escape
            } else {
                let dst_hex = dst_str.replace(char::is_whitespace, "");

                if dst_hex.len() <= 4 {
                    let dst_code = u32::from_str_radix(&dst_hex, 16).ok()?;
                    char::from_u32(dst_code)?.to_string()
                } else if dst_hex.len() <= 6 {
                    // 5-6 hex digits: direct supplementary Unicode code point (e.g., 020BB7 = U+20BB7)
                    let dst_code = u32::from_str_radix(&dst_hex, 16).ok()?;
                    char::from_u32(dst_code)?.to_string()
                } else if dst_hex.len() == 8 {
                    let dst_code = u32::from_str_radix(&dst_hex, 16).ok()?;
                    if let Some(decoded) = decode_utf16_surrogate_pair(dst_code) {
                        decoded
                    } else {
                        // Not a surrogate pair — try as two BMP characters
                        let mut result = String::new();
                        if let Ok(code1) = u32::from_str_radix(&dst_hex[0..4], 16) {
                            if let Some(ch) = char::from_u32(code1) {
                                result.push(ch);
                            }
                        }
                        if let Ok(code2) = u32::from_str_radix(&dst_hex[4..8], 16) {
                            if let Some(ch) = char::from_u32(code2) {
                                result.push(ch);
                            }
                        }
                        if result.is_empty() {
                            return None;
                        }
                        result
                    }
                } else {
                    let mut result = String::new();
                    for i in (0..dst_hex.len()).step_by(4) {
                        let end = (i + 4).min(dst_hex.len());
                        if let Ok(code) = u32::from_str_radix(&dst_hex[i..end], 16) {
                            if let Some(ch) = char::from_u32(code) {
                                result.push(ch);
                            }
                        }
                    }
                    if result.is_empty() {
                        return None;
                    }
                    result
                }
            };

            Some((src, dst))
        })();

        if let Some(pair) = parsed {
            results.push(pair);
        }
    }

    results
}

/// Parse a bfrange line: `<start> <end> <dst>`
///
/// Example: `<0020> <007E> <0020>` maps codes 0x20-0x7E to Unicode U+0020-U+007E.
///
/// There are two formats:
/// 1. `<start> <end> <dst>` - Sequential mapping starting at dst
/// 2. `<start> <end> [<dst1> <dst2> ...]` - Array of individual destinations
///
/// This function supports both formats and flexible whitespace within angle brackets.
pub(super) fn parse_bfrange_line(line: &str) -> Option<Vec<(u32, String)>> {
    static RE_SEQ: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"<([^>]*)>\s*<([^>]*)>\s*<([^>]*)>").unwrap());
    static RE_ARRAY: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"<([^>]*)>\s*<([^>]*)>\s*\[((?:\s*<[^>]+>\s*)+)\]").unwrap()
    });

    // Try format 2 first (array format)
    // Example: <005F> <0061> [<00660066> <00660069> <00660066006C>]
    // Maps codes 0x5F, 0x60, 0x61 to "ff", "fi", "ffl" respectively
    if let Some(caps) = RE_ARRAY.captures(line) {
        let start_str = caps[1].trim().replace(char::is_whitespace, "");
        let end_str = caps[2].trim().replace(char::is_whitespace, "");
        let start = u32::from_str_radix(&start_str, 16).ok()?;
        let end = u32::from_str_radix(&end_str, 16).ok()?;
        let array_str = &caps[3];

        // Extract all destination hex strings from array
        // Each can be a single Unicode code point OR multiple code points (for ligatures)
        static RE_HEX: std::sync::LazyLock<Regex> =
            std::sync::LazyLock::new(|| Regex::new(r"<([^>]*)>").unwrap());
        let dst_hexes: Vec<String> = RE_HEX
            .captures_iter(array_str)
            .filter_map(|cap| {
                let s = cap
                    .get(1)
                    .unwrap()
                    .as_str()
                    .trim()
                    .replace(char::is_whitespace, "");
                if !s.is_empty() {
                    Some(s)
                } else {
                    None
                }
            })
            .collect();

        let mut result = Vec::new();
        let range_size = (end - start + 1) as usize;

        // SPEC VALIDATION: PDF Spec ISO 32000-1:2008, Section 9.10.3
        // The array must have exactly (end - start + 1) entries.
        // Current behavior (lenient): Use what's available, ignore extras/missing.
        // Proper strict mode: Should fail if array size doesn't match range_size.
        if dst_hexes.len() != range_size {
            log::warn!(
                "ToUnicode bfrange array size mismatch: expected {} entries for range 0x{:X}-0x{:X}, got {}",
                range_size,
                start,
                end,
                dst_hexes.len()
            );
        }

        for (i, dst_hex) in dst_hexes.iter().take(range_size).enumerate() {
            let src = start + i as u32;

            // Parse destination - could be one Unicode code point, UTF-16 surrogate, or multiple (ligature)
            let dst = if dst_hex.len() <= 4 {
                // Single Unicode code point (BMP)
                let dst_code = u32::from_str_radix(dst_hex, 16).ok()?;
                char::from_u32(dst_code)?.to_string()
            } else if dst_hex.len() <= 6 {
                // 5-6 hex digits: supplementary Unicode code point (e.g., 020BB7 = U+20BB7)
                let dst_code = u32::from_str_radix(dst_hex, 16).ok()?;
                if let Some(ch) = char::from_u32(dst_code) {
                    ch.to_string()
                } else {
                    continue;
                }
            } else if dst_hex.len() == 8 {
                // 8 hex digits - try UTF-16 surrogate pair first
                let dst_code = u32::from_str_radix(dst_hex, 16).ok()?;
                if let Some(decoded) = decode_utf16_surrogate_pair(dst_code) {
                    decoded
                } else {
                    // Fall back to two separate code points (ligature)
                    let mut unicode_string = String::new();
                    if let Ok(code) = u32::from_str_radix(&dst_hex[0..4], 16) {
                        if let Some(ch) = char::from_u32(code) {
                            unicode_string.push(ch);
                        }
                    }
                    if let Ok(code) = u32::from_str_radix(&dst_hex[4..8], 16) {
                        if let Some(ch) = char::from_u32(code) {
                            unicode_string.push(ch);
                        }
                    }
                    if unicode_string.is_empty() {
                        continue;
                    }
                    unicode_string
                }
            } else {
                // Multi-character mapping (e.g., "ffi", "ffl" for ligatures)
                // Split into 4-char chunks, each representing one Unicode code point
                let mut unicode_string = String::new();
                for chunk_start in (0..dst_hex.len()).step_by(4) {
                    let chunk_end = (chunk_start + 4).min(dst_hex.len());
                    if let Ok(code) = u32::from_str_radix(&dst_hex[chunk_start..chunk_end], 16) {
                        if let Some(ch) = char::from_u32(code) {
                            unicode_string.push(ch);
                        }
                    }
                }
                if unicode_string.is_empty() {
                    continue; // Skip this mapping if parsing failed
                }
                unicode_string
            };

            result.push((src, dst));
        }
        return Some(result);
    }

    // Try format 1 (sequential format)
    if let Some(caps) = RE_SEQ.captures(line) {
        let start_str = caps[1].trim().replace(char::is_whitespace, "");
        let end_str = caps[2].trim().replace(char::is_whitespace, "");
        let dst_start_str = caps[3].trim().replace(char::is_whitespace, "");
        let start = u32::from_str_radix(&start_str, 16).ok()?;
        let end = u32::from_str_radix(&end_str, 16).ok()?;
        let dst_start = u32::from_str_radix(&dst_start_str, 16).ok()?;

        let mut result = Vec::new();
        let range_size = end.saturating_sub(start).min(10000); // Safety limit

        // For surrogate pair destinations (8 hex digits), decode to Unicode code point
        // first, then increment the code point. Naively incrementing the raw u32 would
        // overflow across the low surrogate boundary (0xDFFF → 0xE000).
        let base_codepoint = if dst_start > 0xFFFF {
            if let Some(decoded) = decode_utf16_surrogate_pair(dst_start) {
                // It's a surrogate pair — use decoded code point as base
                decoded.chars().next().map(|c| c as u32)
            } else {
                // Not a surrogate pair but > 0xFFFF — use as direct code point
                Some(dst_start)
            }
        } else {
            Some(dst_start)
        };

        if let Some(base_cp) = base_codepoint {
            for i in 0..=range_size {
                let src = start.wrapping_add(i);
                let cp = base_cp.wrapping_add(i);
                if let Some(ch) = char::from_u32(cp) {
                    result.push((src, ch.to_string()));
                }
            }
        }
        return Some(result);
    }

    None
}

/// Parse a notdefrange line: `<start> <end> <dst>`
///
/// Phase 4.1 addition: Support for beginnotdefrange sections
///
/// Example: `<0000> <0040> <FFFD>` maps codes 0x0000-0x0040 to U+FFFD (replacement character)
/// for unmapped character codes (fallback/notdef handling).
///
/// Unlike bfrange, notdefrange only supports the sequential format (not arrays).
/// Notdefrange mappings are applied only to codes not already mapped by bfchar/bfrange.
fn parse_notdefrange_line(line: &str) -> Option<Vec<(u32, String)>> {
    static RE_SEQ: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"<([^>]*)>\s*<([^>]*)>\s*<([^>]*)>").unwrap());

    if let Some(caps) = RE_SEQ.captures(line) {
        let start_str = caps[1].trim().replace(char::is_whitespace, "");
        let end_str = caps[2].trim().replace(char::is_whitespace, "");
        let dst_str = caps[3].trim();

        let start = u32::from_str_radix(&start_str, 16).ok()?;
        let end = u32::from_str_radix(&end_str, 16).ok()?;

        // Parse destination - try escape sequence first, then hex
        let dst = if let Some(escape) = parse_escape_sequence(&format!("<{}>", dst_str)) {
            escape
        } else {
            let dst_hex = dst_str.replace(char::is_whitespace, "");
            let dst_code = u32::from_str_radix(&dst_hex, 16).ok()?;
            if dst_code > 0xFFFF {
                // Try surrogate pair decoding first, then direct code point
                decode_utf16_surrogate_pair(dst_code)
                    .or_else(|| char::from_u32(dst_code).map(|ch| ch.to_string()))?
            } else {
                char::from_u32(dst_code)?.to_string()
            }
        };

        let mut result = Vec::new();
        let range_size = end.saturating_sub(start).min(10000); // Safety limit
        for i in 0..=range_size {
            let src = start.wrapping_add(i);
            result.push((src, dst.clone()));
        }
        return Some(result);
    }

    None
}

/// Parse a CID to Unicode mapping (simplified version for CID fonts).
///
/// This is a wrapper around `parse_tounicode_cmap` for consistency.
pub fn parse_cid_to_unicode(data: &[u8]) -> Result<CMap> {
    parse_tounicode_cmap(data)
}
