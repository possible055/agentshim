use super::*;

/// Parse N float operands from a raw byte slice.
/// Returns a fixed-size array. Returns None if not enough parseable numbers.
#[inline]
pub(super) fn parse_floats<const N: usize>(data: &[u8]) -> Option<[f32; N]> {
    let s = std::str::from_utf8(data).ok()?;
    let mut iter = s.split_ascii_whitespace();
    let mut result = [0.0f32; N];
    for val in &mut result {
        *val = iter.next()?.parse::<f32>().ok()?;
    }
    Some(result)
}

/// Parse 6 float operands from a raw byte slice (for inline `cm` parsing).
/// Returns None if the slice doesn't contain exactly 6 parseable numbers.
#[inline]
pub(super) fn parse_six_floats(data: &[u8]) -> Option<(f32, f32, f32, f32, f32, f32)> {
    let f = parse_floats::<6>(data)?;
    Some((f[0], f[1], f[2], f[3], f[4], f[5]))
}

/// Byte-level check for pure graphics operators that can be skipped during
/// text-only extraction. Equivalent to [`is_skippable_graphics_op`] but
/// operates on raw `&[u8]` without UTF-8 conversion.
///
/// Includes the colour operators (rg/RG/g/G/k/K/cs/CS/sc/SC/scn/SCN):
/// skipping them here is only sound when the caller also guarantees a
/// matching `Q` will revert any colour change before it can reach a `BT`
/// (the `deferred_depth > 0` block in `scan_graphics_region`). Do not use
/// this predicate to decide skippability outside a deferred q/Q scope -
/// see [`is_color_op_bytes`] for that case.
pub(super) fn is_skippable_graphics_op_bytes(op: &[u8]) -> bool {
    matches!(
        op,
        b"m" | b"l" | b"c" | b"v" | b"y" | b"h" | b"re"       // path construction
        | b"S" | b"s" | b"f" | b"F" | b"f*"                     // path painting
        | b"B" | b"B*" | b"b" | b"b*" | b"n"                    // path painting (fill+stroke)
        | b"W" | b"W*"                                           // clipping
        | b"w" | b"J" | b"j" | b"M" | b"d" | b"i" | b"ri" | b"sh" // non-text graphics state
        | b"rg" | b"RG" | b"g" | b"G" | b"k" | b"K"            // color (rgb/gray/cmyk)
        | b"cs" | b"CS" | b"sc" | b"SC" | b"scn" | b"SCN" // color space/components
    )
}

/// Byte-level check for operators that set persistent fill/stroke colour
/// state (rg/RG/g/G/k/K/cs/CS/sc/SC/scn/SCN).
///
/// Used at the *top level* of `scan_graphics_region` (deferred_depth == 0,
/// i.e. no enclosing unmatched `q`). Unlike pure path/paint/clip operators,
/// a colour change here is not reverted by anything before the next `BT` -
/// per ISO 32000-1:2008 SS8.4 the graphics state (including colour) persists
/// across BT/ET boundaries. Discarding it as "skippable" left GraphicsState
/// stuck at its default black whenever a document set fill colour before
/// opening the text object (a common pattern: BDC, colour, BT, Tf, Tm, Tj),
/// so text that should render in colour was extracted as black even though
/// the identical scn issued *inside* an already-open BT worked correctly.
pub(super) fn is_color_op_bytes(op: &[u8]) -> bool {
    matches!(
        op,
        b"rg" | b"RG" | b"g" | b"G" | b"k" | b"K" | b"cs" | b"CS" | b"sc" | b"SC" | b"scn" | b"SCN"
    )
}

// ── Raw index-returning skip functions ─────────────────────────────────────
//
// Same logic as the nom-based skip_*() functions above, but return a new
// index position instead of IResult. On malformed input, Option variants
// return None so the caller can skip one byte (matching current error
// recovery).

/// Parse a float directly from bytes. Returns (value, bytes_consumed).
#[inline]
pub(super) fn parse_float_fast(data: &[u8]) -> Option<(f32, usize)> {
    let mut i = 0;
    let negative = if i < data.len() && (data[i] == b'-' || data[i] == b'+') {
        let neg = data[i] == b'-';
        i += 1;
        neg
    } else {
        false
    };

    let start = i;
    let mut int_part: f64 = 0.0;
    while i < data.len() && data[i].is_ascii_digit() {
        int_part = int_part * 10.0 + (data[i] - b'0') as f64;
        i += 1;
    }

    let mut frac_part: f64 = 0.0;
    let mut frac_scale: f64 = 1.0;
    if i < data.len() && data[i] == b'.' {
        i += 1;
        while i < data.len() && data[i].is_ascii_digit() {
            frac_part = frac_part * 10.0 + (data[i] - b'0') as f64;
            frac_scale *= 10.0;
            i += 1;
        }
    }

    if i == start {
        return None; // no digits consumed
    }

    let value = int_part + frac_part / frac_scale;
    let value = if negative { -value } else { value };
    Some((value as f32, i))
}

/// Parse a literal string `(...)` from bytes. Returns (decoded_bytes, position_after_close_paren).
#[inline]
pub(super) fn parse_literal_string_fast(data: &[u8], start: usize) -> Option<(Vec<u8>, usize)> {
    let mut i = start + 1; // past opening '('
    let mut depth: u32 = 1;

    // Fast path: scan for simple strings without escapes or nested parens.
    // Most PDF strings are simple ASCII text like "(Hello)" or single chars like "(A)".
    let scan_start = i;
    while i < data.len() {
        match data[i] {
            b')' => {
                // Simple string — no escapes, no nesting
                return Some((data[scan_start..i].to_vec(), i + 1));
            }
            b'\\' | b'(' => break, // needs complex handling
            _ => i += 1,
        }
    }

    // Slow path: string has escapes or nested parens
    i = scan_start;
    let mut result = Vec::new();
    while i < data.len() && depth > 0 {
        match data[i] {
            b'\\' if i + 1 < data.len() => {
                match data[i + 1] {
                    b'n' => {
                        result.push(b'\n');
                        i += 2;
                    }
                    b'r' => {
                        result.push(b'\r');
                        i += 2;
                    }
                    b't' => {
                        result.push(b'\t');
                        i += 2;
                    }
                    b'b' => {
                        result.push(0x08);
                        i += 2;
                    }
                    b'f' => {
                        result.push(0x0C);
                        i += 2;
                    }
                    b'(' => {
                        result.push(b'(');
                        i += 2;
                    }
                    b')' => {
                        result.push(b')');
                        i += 2;
                    }
                    b'\\' => {
                        result.push(b'\\');
                        i += 2;
                    }
                    b'0'..=b'7' => {
                        // Octal escape
                        let mut octal: u32 = (data[i + 1] - b'0') as u32;
                        let mut j = i + 2;
                        for _ in 0..2 {
                            if j < data.len() && (b'0'..=b'7').contains(&data[j]) {
                                octal = octal * 8 + (data[j] - b'0') as u32;
                                j += 1;
                            } else {
                                break;
                            }
                        }
                        result.push((octal & 0xFF) as u8);
                        i = j;
                    }
                    b'\r' => {
                        i += 2;
                        if i < data.len() && data[i] == b'\n' {
                            i += 1;
                        }
                    }
                    b'\n' => {
                        i += 2;
                    }
                    _ => {
                        result.push(data[i + 1]);
                        i += 2;
                    }
                }
            }
            b'(' => {
                depth += 1;
                result.push(b'(');
                i += 1;
            }
            b')' => {
                depth -= 1;
                if depth > 0 {
                    result.push(b')');
                }
                i += 1;
            }
            _ => {
                result.push(data[i]);
                i += 1;
            }
        }
    }
    if depth == 0 {
        Some((result, i))
    } else {
        None
    }
}

/// Parse a hex string `<...>` from bytes. Returns (decoded_bytes, position_after_close_angle).
#[inline]
pub(super) fn parse_hex_string_fast(data: &[u8], start: usize) -> Option<(Vec<u8>, usize)> {
    let mut i = start + 1; // past opening '<'
    let mut result = Vec::new();
    let mut high_nibble: Option<u8> = None;
    while i < data.len() {
        let b = data[i];
        if b == b'>' {
            // If odd number of hex digits, append 0 to make final byte
            if let Some(h) = high_nibble {
                result.push(h << 4);
            }
            return Some((result, i + 1));
        }
        if let Some(nibble) = hex_nibble(b) {
            match high_nibble {
                None => high_nibble = Some(nibble),
                Some(h) => {
                    result.push((h << 4) | nibble);
                    high_nibble = None;
                }
            }
        }
        // Skip whitespace and other non-hex chars
        i += 1;
    }
    None
}

#[inline]
pub(super) fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Parse a TJ array `[...]` from bytes. Returns (elements, position_after_close_bracket).
pub(super) fn parse_tj_array_fast(data: &[u8], start: usize) -> Option<(Vec<TextElement>, usize)> {
    let mut i = start + 1; // past opening '['
    let mut elements = Vec::new();
    loop {
        // Skip whitespace
        while i < data.len() && is_whitespace(data[i]) {
            i += 1;
        }
        if i >= data.len() {
            return None;
        }

        match data[i] {
            b']' => return Some((elements, i + 1)),
            b'(' => {
                if let Some((bytes, end)) = parse_literal_string_fast(data, i) {
                    elements.push(TextElement::String(bytes));
                    i = end;
                } else {
                    return None;
                }
            }
            b'<' => {
                if let Some((bytes, end)) = parse_hex_string_fast(data, i) {
                    elements.push(TextElement::String(bytes));
                    i = end;
                } else {
                    return None;
                }
            }
            b'0'..=b'9' | b'.' | b'+' | b'-' => {
                if let Some((num, consumed)) = parse_float_fast(&data[i..]) {
                    elements.push(TextElement::Offset(num));
                    i += consumed;
                } else {
                    return None;
                }
            }
            _ => {
                // Skip unknown token
                i += 1;
            }
        }
    }
}

/// Parse a name `/Name` from bytes. Returns (name_string, position_after_name).
#[inline]
pub(super) fn parse_name_fast(data: &[u8], start: usize) -> (String, usize) {
    let mut i = start + 1; // past '/'
    let name_start = i;
    while i < data.len() && !is_whitespace_or_delimiter(data[i]) {
        i += 1;
    }
    let name = String::from_utf8_lossy(&data[name_start..i]).to_string();
    (name, i)
}
