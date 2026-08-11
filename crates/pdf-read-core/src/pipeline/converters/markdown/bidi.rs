use super::*;

/// Walk `text` line by line and wrap each line's RTL runs (or LTR
/// runs inside RTL-dominant lines) with Unicode bidi-isolation
/// markers per UAX #9 §2.4. Pure-LTR lines (no RTL chars) are
/// returned unchanged byte-for-byte.
///
/// Block direction is decided per *line* because markdown line
/// breaks (`\n`) implicitly start a new bidi paragraph in every
/// viewer that honours UAX #9. We use
/// [`crate::text::bidi::paragraph_is_rtl`] which follows §3.3.1
/// (first-strong-character rule).
///
/// Trailing newlines are preserved (`str::lines()` would otherwise
/// drop them) so the document-level newline shape stays intact.
pub(super) fn wrap_bidi_isolates_per_line(text: &str) -> String {
    let trailing_newlines: String = text
        .chars()
        .rev()
        .take_while(|&c| c == '\n')
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    let lines: Vec<&str> = text.lines().collect();
    let mut out = String::with_capacity(text.len() + 16);
    for (i, line) in lines.iter().enumerate() {
        if crate::text::bidi::looks_rtl(line) {
            let block_is_rtl = crate::text::bidi::paragraph_is_rtl(line);
            out.push_str(&crate::text::bidi::wrap_rtl_isolates(line, block_is_rtl));
        } else {
            out.push_str(line);
        }
        if i + 1 < lines.len() {
            out.push('\n');
        }
    }
    if !trailing_newlines.is_empty() {
        out.push_str(&trailing_newlines);
    }
    out
}

/// Remove markdown `**` and `*` emphasis pairs that surround RTL
/// (Arabic / Hebrew) tokens. Inserted by the bold/italic detector
/// when the source PDF reports a font-weight change between
/// contextual glyph forms (initial / medial / final shapes); they
/// fragment the line into spurious emphasis spans and break bidi
/// reordering. Keeps emphasis around purely LTR runs intact.
///
/// Implementation note: the byte-position search via
/// `find_matching` is safe even on multi-byte UTF-8 because we only
/// look for ASCII `*` (0x2A) which never appears as a continuation
/// byte; matched indices always fall on a UTF-8 boundary. We then
/// build the output by appending UTF-8 string slices between the
/// matched positions, never reinterpreting individual bytes as
/// chars. (Copilot review #3108056051: the previous implementation
/// emitted `bytes[i] as char` for non-marker bytes and corrupted
/// non-ASCII content like `בנימין * world` → `×<ctrl>×<ctrl>... * world`.)
pub(super) fn strip_inline_emphasis_in_rtl(line: &str) -> String {
    // Cheap path: if there are no asterisks, nothing to do.
    if !line.contains('*') {
        return line.to_string();
    }
    let bytes = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    let mut last_copy = 0;
    while i < bytes.len() {
        // Try to match `**` first.
        if i + 1 < bytes.len() && bytes[i] == b'*' && bytes[i + 1] == b'*' {
            if let Some(close) = find_matching(bytes, i + 2, b"**") {
                // Copy any text that came before this `**` token verbatim.
                if i > last_copy {
                    out.push_str(&line[last_copy..i]);
                }
                let inner = &line[i + 2..close];
                if crate::text::bidi::looks_rtl(inner) {
                    out.push_str(inner);
                } else {
                    out.push_str("**");
                    out.push_str(inner);
                    out.push_str("**");
                }
                i = close + 2;
                last_copy = i;
                continue;
            }
        }
        // Then `*` (italic).
        if bytes[i] == b'*' {
            if let Some(close) = find_matching(bytes, i + 1, b"*") {
                if i > last_copy {
                    out.push_str(&line[last_copy..i]);
                }
                let inner = &line[i + 1..close];
                if crate::text::bidi::looks_rtl(inner) {
                    out.push_str(inner);
                } else {
                    out.push('*');
                    out.push_str(inner);
                    out.push('*');
                }
                i = close + 1;
                last_copy = i;
                continue;
            }
        }
        i += 1;
    }
    if last_copy < bytes.len() {
        out.push_str(&line[last_copy..]);
    }
    out
}

pub(super) fn find_matching(bytes: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    let mut i = from;
    while i + needle.len() <= bytes.len() {
        if &bytes[i..i + needle.len()] == needle {
            return Some(i);
        }
        i += 1;
    }
    None
}
