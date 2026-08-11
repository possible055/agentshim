use super::*;

pub(super) fn skip_literal_string_raw(data: &[u8], mut i: usize) -> Option<usize> {
    i += 1; // past opening '('
    let mut depth: u32 = 1;
    while i < data.len() && depth > 0 {
        match data[i] {
            b'\\' if i + 1 < data.len() => i += 2,
            b'(' => {
                depth += 1;
                i += 1;
            }
            b')' => {
                depth -= 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    if depth == 0 {
        Some(i)
    } else {
        None
    }
}

pub(super) fn skip_hex_string_raw(data: &[u8], mut i: usize) -> Option<usize> {
    i += 1; // past opening '<'
    while i < data.len() {
        if data[i] == b'>' {
            return Some(i + 1);
        }
        i += 1;
    }
    None
}

#[inline]
pub(super) fn skip_name_raw(data: &[u8], mut i: usize) -> usize {
    i += 1; // past '/'
    while i < data.len() && !is_whitespace_or_delimiter(data[i]) {
        i += 1;
    }
    i
}

pub(super) fn skip_array_raw(data: &[u8], i: usize) -> Option<usize> {
    let mut pos = i + 1; // past opening '['
    let mut depth: u32 = 1;
    while pos < data.len() && depth > 0 {
        match data[pos] {
            b'[' => {
                depth += 1;
                pos += 1;
            }
            b']' => {
                depth -= 1;
                pos += 1;
            }
            b'(' => {
                pos += 1;
                let mut str_depth: u32 = 1;
                while pos < data.len() && str_depth > 0 {
                    match data[pos] {
                        b'\\' if pos + 1 < data.len() => pos += 2,
                        b'(' => {
                            str_depth += 1;
                            pos += 1;
                        }
                        b')' => {
                            str_depth -= 1;
                            pos += 1;
                        }
                        _ => pos += 1,
                    }
                }
            }
            b'<' if pos + 1 < data.len() && data[pos + 1] == b'<' => {
                pos += 2;
                let mut dict_depth: u32 = 1;
                while pos + 1 < data.len() && dict_depth > 0 {
                    if data[pos] == b'<' && data[pos + 1] == b'<' {
                        dict_depth += 1;
                        pos += 2;
                    } else if data[pos] == b'>' && data[pos + 1] == b'>' {
                        dict_depth -= 1;
                        pos += 2;
                    } else {
                        pos += 1;
                    }
                }
            }
            b'<' => {
                pos += 1;
                while pos < data.len() && data[pos] != b'>' {
                    pos += 1;
                }
                if pos < data.len() {
                    pos += 1;
                }
            }
            _ => pos += 1,
        }
    }
    if depth == 0 {
        Some(pos)
    } else {
        None
    }
}

pub(super) fn skip_dict_raw(data: &[u8], i: usize) -> Option<usize> {
    let mut pos = i + 2; // past opening '<<'
    let mut depth: u32 = 1;
    while pos < data.len() && depth > 0 {
        if pos + 1 < data.len() && data[pos] == b'<' && data[pos + 1] == b'<' {
            depth += 1;
            pos += 2;
        } else if pos + 1 < data.len() && data[pos] == b'>' && data[pos + 1] == b'>' {
            depth -= 1;
            pos += 2;
        } else if data[pos] == b'(' {
            pos += 1;
            let mut str_depth: u32 = 1;
            while pos < data.len() && str_depth > 0 {
                match data[pos] {
                    b'\\' if pos + 1 < data.len() => pos += 2,
                    b'(' => {
                        str_depth += 1;
                        pos += 1;
                    }
                    b')' => {
                        str_depth -= 1;
                        pos += 1;
                    }
                    _ => pos += 1,
                }
            }
        } else if data[pos] == b'<' {
            pos += 1;
            while pos < data.len() && data[pos] != b'>' {
                pos += 1;
            }
            if pos < data.len() {
                pos += 1;
            }
        } else {
            pos += 1;
        }
    }
    if depth == 0 {
        Some(pos)
    } else {
        None
    }
}

// ── Fast BT/ET block parser ────────────────────────────────────────────
//
// Hand-written byte-level parser for operators inside text blocks.
// Avoids the nom tokenizer overhead (~3-5x faster than parse_operator_with_operands)
// by parsing numbers inline, skipping indirect-reference lookahead, and matching
// operator names as raw bytes.

pub(super) const SCAN_SKIP: u8 = 0;

pub(super) const SCAN_ALPHA: u8 = 1;

pub(super) const SCAN_PAREN: u8 = 2;

pub(super) const SCAN_ANGLE: u8 = 3;

pub(super) const SCAN_BRACKET: u8 = 4;

pub(super) const SCAN_SLASH: u8 = 5;

pub(super) const SCAN_PERCENT: u8 = 6;

pub(super) const SCAN_OTHER: u8 = 7;

pub(super) static BYTE_CLASS: [u8; 256] = {
    let mut t = [SCAN_OTHER; 256];
    // Whitespace
    t[b' ' as usize] = SCAN_SKIP;
    t[b'\t' as usize] = SCAN_SKIP;
    t[b'\n' as usize] = SCAN_SKIP;
    t[b'\r' as usize] = SCAN_SKIP;
    t[0x00] = SCAN_SKIP; // null
    t[0x0C] = SCAN_SKIP; // form feed
                         // Digits
    t[b'0' as usize] = SCAN_SKIP;
    t[b'1' as usize] = SCAN_SKIP;
    t[b'2' as usize] = SCAN_SKIP;
    t[b'3' as usize] = SCAN_SKIP;
    t[b'4' as usize] = SCAN_SKIP;
    t[b'5' as usize] = SCAN_SKIP;
    t[b'6' as usize] = SCAN_SKIP;
    t[b'7' as usize] = SCAN_SKIP;
    t[b'8' as usize] = SCAN_SKIP;
    t[b'9' as usize] = SCAN_SKIP;
    // Number punctuation
    t[b'.' as usize] = SCAN_SKIP;
    t[b'+' as usize] = SCAN_SKIP;
    t[b'-' as usize] = SCAN_SKIP;
    // Alpha (uppercase)
    let mut c = b'A';
    while c <= b'Z' {
        t[c as usize] = SCAN_ALPHA;
        c += 1;
    }
    // Alpha (lowercase)
    c = b'a';
    while c <= b'z' {
        t[c as usize] = SCAN_ALPHA;
        c += 1;
    }
    // Quote/star operators
    t[b'\'' as usize] = SCAN_ALPHA;
    t[b'"' as usize] = SCAN_ALPHA;
    t[b'*' as usize] = SCAN_ALPHA;
    // Delimiters
    t[b'(' as usize] = SCAN_PAREN;
    t[b'<' as usize] = SCAN_ANGLE;
    t[b'[' as usize] = SCAN_BRACKET;
    t[b'/' as usize] = SCAN_SLASH;
    t[b'%' as usize] = SCAN_PERCENT;
    t
};

pub(super) fn scan_graphics_region<'a>(
    data: &'a [u8],
    consecutive_errors: &mut usize,
) -> ScanResult<'a> {
    let mut i: usize = 0;
    let mut operand_start: usize = 0;
    let mut deferred_depth: u32 = 0;
    let mut deferred_start: usize = 0;
    let len = data.len();

    loop {
        // Bulk-skip whitespace, digits, dots, signs — the most common bytes in graphics streams
        while i < len && BYTE_CLASS[data[i] as usize] == SCAN_SKIP {
            i += 1;
        }
        if i >= len {
            return ScanResult::EndOfData;
        }

        match BYTE_CLASS[data[i] as usize] {
            SCAN_ALPHA => {
                let first_byte = data[i];
                let second_is_non_alpha =
                    i + 1 >= len || BYTE_CLASS[data[i + 1] as usize] != SCAN_ALPHA;

                // Fast path for common single-char skippable operators.
                // Avoids reading the full operator name and is_skippable check.
                // Path: m(moveto), l(lineto), c(curveto), v/y(curves), h(close)
                // Paint: f/F(fill), B/b(fill+stroke), S/s(stroke), n(endpath), W(clip)
                // State: w(linewidth), d(dash), i(flatness), J/j(cap/join), M(miter)
                // Note: q/Q excluded (need deferred depth tracking). g/G/k/K
                // (gray/cmyk fill-color) also excluded - they mutate persistent
                // colour state that must reach a later BT/Tj when found outside
                // a q/Q scope (see is_color_op_bytes below); they fall through to
                // the slow alpha-scan path so that check can see them.
                if second_is_non_alpha
                    && matches!(
                        first_byte,
                        b'm' | b'l'
                            | b'c'
                            | b'v'
                            | b'y'
                            | b'h'
                            | b'f'
                            | b'F'
                            | b'B'
                            | b'b'
                            | b'S'
                            | b's'
                            | b'n'
                            | b'W'
                            | b'w'
                            | b'd'
                            | b'i'
                            | b'J'
                            | b'j'
                            | b'M'
                    )
                {
                    i += 1;
                    *consecutive_errors = 0;
                    operand_start = i;
                    continue;
                }

                let op_start = i;
                while i < len
                    && (data[i].is_ascii_alphanumeric()
                        || data[i] == b'\''
                        || data[i] == b'"'
                        || data[i] == b'*')
                {
                    i += 1;
                }
                let op = &data[op_start..i];

                // Keyword operands — not operators
                if op == b"true" || op == b"false" || op == b"null" {
                    *consecutive_errors = 0;
                    continue;
                }

                *consecutive_errors = 0;

                if op == b"q" {
                    if deferred_depth == 0 {
                        deferred_start = operand_start;
                    }
                    deferred_depth += 1;
                    operand_start = i;
                    continue;
                } else if op == b"Q" {
                    if deferred_depth > 0 {
                        deferred_depth -= 1;
                        operand_start = i;
                        continue;
                    }
                    // Unmatched Q outside deferred — emit directly.
                    // Q has no operands; NeedFullParse invokes full nom parser
                    // for a trivial no-operand op (116K triggers for Penrose).
                    return ScanResult::SimpleOp {
                        op: Operator::RestoreState,
                        rest: &data[i..],
                    };
                } else if deferred_depth > 0 {
                    // Inside a deferred q block — check if this op needs flushing
                    if op == b"cm" || op == b"gs" || is_skippable_graphics_op_bytes(op) {
                        operand_start = i;
                        continue;
                    }
                    return ScanResult::DeferredThenText {
                        deferred_start: &data[deferred_start..],
                        trigger_start: &data[operand_start..],
                    };
                } else if op == b"BT" {
                    return ScanResult::FoundBT { rest: &data[i..] };
                } else if op == b"BI" {
                    return ScanResult::InlineImage { rest: &data[i..] };
                } else if op == b"cm" {
                    // ConcatMatrix: parse 6 floats inline to avoid nom overhead
                    // (171K triggers/PDF for Murphy). Falls back to NeedFullParse
                    // on malformed operands.
                    if let Some((a, b, c, d, e, f)) =
                        parse_six_floats(&data[operand_start..op_start])
                    {
                        return ScanResult::SimpleOp {
                            op: Operator::Cm { a, b, c, d, e, f },
                            rest: &data[i..],
                        };
                    }
                    return ScanResult::NeedFullParse {
                        operand_start: &data[operand_start..],
                        after_op: &data[i..],
                    };
                } else if is_color_op_bytes(op) {
                    // Outside any q/Q scope (deferred_depth == 0) nothing
                    // will revert this colour change before the next BT -
                    // route through the full parser so the handler applies
                    // it to GraphicsState instead of silently dropping it.
                    return ScanResult::NeedFullParse {
                        operand_start: &data[operand_start..],
                        after_op: &data[i..],
                    };
                } else if is_skippable_graphics_op_bytes(op) {
                    operand_start = i;
                    continue;
                } else {
                    return ScanResult::NeedFullParse {
                        operand_start: &data[operand_start..],
                        after_op: &data[i..],
                    };
                }
            }

            SCAN_PAREN => match skip_literal_string_raw(data, i) {
                Some(end) => {
                    i = end;
                    *consecutive_errors = 0;
                }
                None => {
                    i += 1;
                    *consecutive_errors += 1;
                }
            },

            SCAN_ANGLE => {
                if i + 1 < len && data[i + 1] == b'<' {
                    match skip_dict_raw(data, i) {
                        Some(end) => {
                            i = end;
                            *consecutive_errors = 0;
                        }
                        None => {
                            i += 1;
                            *consecutive_errors += 1;
                        }
                    }
                } else {
                    match skip_hex_string_raw(data, i) {
                        Some(end) => {
                            i = end;
                            *consecutive_errors = 0;
                        }
                        None => {
                            i += 1;
                            *consecutive_errors += 1;
                        }
                    }
                }
            }

            SCAN_BRACKET => match skip_array_raw(data, i) {
                Some(end) => {
                    i = end;
                    *consecutive_errors = 0;
                }
                None => {
                    i += 1;
                    *consecutive_errors += 1;
                }
            },

            SCAN_SLASH => {
                i = skip_name_raw(data, i);
                *consecutive_errors = 0;
            }

            SCAN_PERCENT => {
                while i < len && data[i] != b'\n' && data[i] != b'\r' {
                    i += 1;
                }
                *consecutive_errors = 0;
            }

            _ => {
                i += 1;
                *consecutive_errors += 1;
            }
        }

        if *consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
            return ScanResult::TooManyErrors {
                remaining: &data[i..],
            };
        }
    }
}
