use super::*;

/// Lightweight forward scan that tracks graphics state across the full content stream.
///
/// Scans the stream recognizing only `q`, `Q`, `cm`, and `Tf` operators, skipping
/// all path, color, and text operators. Records the accumulated CTM and font state
/// at each position in `text_positions`.
///
/// This is much cheaper than full parsing. Numeric operands are tracked in a
/// rolling buffer so `cm` operands are always available when the operator is
/// encountered.
///
/// # Arguments
///
/// * `data` - Raw content stream bytes
/// * `text_positions` - Byte offsets of BT/Do operators to record state at
///
/// # Returns
///
/// One [`PrescanState`] per entry in `text_positions` (same order).
/// Returns `None` if the scan encounters unrecoverable problems.
pub(super) fn forward_scan_ctm(data: &[u8], text_positions: &[usize]) -> Option<Vec<PrescanState>> {
    use crate::content::graphics_state::Matrix;

    if text_positions.is_empty() {
        return Some(Vec::new());
    }

    // Font tracking: store (name, size) pairs in a table, reference by index
    // in ctm_stack to avoid String cloning on every q/Q.
    let mut font_table: Vec<(String, f32)> = Vec::new();
    let mut current_font_idx: Option<usize> = None;
    let mut ctm_stack: Vec<(Matrix, Option<usize>)> = Vec::with_capacity(32);
    let mut ctm = Matrix::identity();

    // Rolling buffer of recent numeric operands (for cm's 6 floats)
    let mut num_buf: [f32; 6] = [0.0; 6];
    let mut num_count: usize = 0;

    // Track last name operand for Tf (font name like /F1)
    let mut last_name: Option<String> = None;

    // Sort positions so we can walk forward and match them in order
    let mut sorted_positions: Vec<(usize, usize)> =
        text_positions.iter().copied().enumerate().collect();
    sorted_positions.sort_by_key(|&(_, pos)| pos);
    let mut next_tp_idx = 0;

    // Results in original order
    let mut results: Vec<PrescanState> = (0..text_positions.len())
        .map(|_| PrescanState {
            ctm: (0.0, 0.0, 0.0, 0.0, 0.0, 0.0),
            font: None,
        })
        .collect();

    let len = data.len();
    let mut i = 0;

    while i < len {
        // Record state at any text positions we've passed
        while next_tp_idx < sorted_positions.len() && sorted_positions[next_tp_idx].1 <= i {
            let (orig_idx, _) = sorted_positions[next_tp_idx];
            results[orig_idx] = PrescanState {
                ctm: (ctm.a, ctm.b, ctm.c, ctm.d, ctm.e, ctm.f),
                font: current_font_idx.map(|idx| font_table[idx].clone()),
            };
            next_tp_idx += 1;
        }

        if next_tp_idx >= sorted_positions.len() {
            break; // All text positions recorded
        }

        let b = data[i];

        // Skip whitespace
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }

        // Parse numeric tokens into the rolling buffer
        if b.is_ascii_digit()
            || b == b'-'
            || b == b'+'
            || (b == b'.' && i + 1 < len && data[i + 1].is_ascii_digit())
        {
            let start = i;
            i += 1;
            while i < len && (data[i].is_ascii_digit() || data[i] == b'.') {
                i += 1;
            }
            if let Ok(val) = std::str::from_utf8(&data[start..i])
                .unwrap_or("")
                .parse::<f32>()
            {
                // Shift buffer left and append
                if num_count < 6 {
                    num_buf[num_count] = val;
                    num_count += 1;
                } else {
                    num_buf.rotate_left(1);
                    num_buf[5] = val;
                }
            }
            continue;
        }

        // Operator detection — only care about q, Q, cm
        if b.is_ascii_alphabetic() {
            let op_start = i;
            i += 1;
            while i < len
                && (data[i].is_ascii_alphabetic()
                    || data[i] == b'*'
                    || data[i] == b'\''
                    || data[i] == b'"')
            {
                i += 1;
            }
            let op = &data[op_start..i];

            match op {
                b"BI" => {
                    // Inline image (§8.9.7): `BI key/val pairs ID
                    // <binary data> EI`. The binary image bytes can
                    // contain stray q/Q/cm-shaped ASCII sequences
                    // that would corrupt the CTM stack if parsed as
                    // operators. Skip the whole block by scanning
                    // to the first whitespace-bounded `EI` (`EI`
                    // embedded inside the binary data is tolerated
                    // because the surrounding bytes are unlikely to
                    // be whitespace).
                    num_count = 0;
                    let mut j = i;
                    while j + 1 < len {
                        if data[j] == b'E' && data[j + 1] == b'I' {
                            let before_ok = j == 0 || data[j - 1].is_ascii_whitespace();
                            let after_ok = j + 2 >= len
                                || data[j + 2].is_ascii_whitespace()
                                || matches!(data[j + 2], b'(' | b'<' | b'[' | b'/' | b'%');
                            if before_ok && after_ok {
                                j += 2;
                                break;
                            }
                        }
                        j += 1;
                    }
                    i = j;
                    continue;
                }
                b"q" => {
                    ctm_stack.push((ctm, current_font_idx));
                    num_count = 0;
                }
                b"Q" => {
                    if let Some((saved_ctm, saved_font_idx)) = ctm_stack.pop() {
                        ctm = saved_ctm;
                        current_font_idx = saved_font_idx;
                    }
                    num_count = 0;
                }
                b"cm" => {
                    if num_count >= 6 {
                        let base = num_count - 6;
                        let new_ctm = Matrix {
                            a: num_buf[base],
                            b: num_buf[base + 1],
                            c: num_buf[base + 2],
                            d: num_buf[base + 3],
                            e: num_buf[base + 4],
                            f: num_buf[base + 5],
                        };
                        ctm = new_ctm.multiply(&ctm);
                    }
                    num_count = 0;
                }
                b"Tf" => {
                    // Font set: last_name has font name, last num has size
                    if num_count >= 1 {
                        let size = num_buf[num_count - 1];
                        if let Some(ref name) = last_name {
                            let idx = font_table.len();
                            font_table.push((name.clone(), size));
                            current_font_idx = Some(idx);
                        }
                    }
                    num_count = 0;
                    last_name = None;
                }
                _ => {
                    num_count = 0;
                }
            }
            continue;
        }

        // Skip string literals to avoid false matches
        if b == b'(' {
            i += 1;
            let mut depth = 1u32;
            while i < len && depth > 0 {
                match data[i] {
                    b'\\' => i += 1, // skip escaped char
                    b'(' => depth += 1,
                    b')' => depth -= 1,
                    _ => {}
                }
                i += 1;
            }
            num_count = 0;
            continue;
        }
        if b == b'<' {
            if i + 1 < len && data[i + 1] == b'<' {
                // Dict << >> — skip to matching >>
                i += 2;
                let mut depth = 1u32;
                while i + 1 < len && depth > 0 {
                    if data[i] == b'<' && data[i + 1] == b'<' {
                        depth += 1;
                        i += 2;
                    } else if data[i] == b'>' && data[i + 1] == b'>' {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
            } else {
                // Hex string <...>
                i += 1;
                while i < len && data[i] != b'>' {
                    i += 1;
                }
                if i < len {
                    i += 1;
                }
            }
            num_count = 0;
            continue;
        }

        // Names (/Foo) — track for Tf font name
        if b == b'/' {
            let name_start = i + 1;
            i += 1;
            while i < len
                && !data[i].is_ascii_whitespace()
                && !matches!(
                    data[i],
                    b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
                )
            {
                i += 1;
            }
            last_name = std::str::from_utf8(&data[name_start..i])
                .ok()
                .map(|s| s.to_string());
            num_count = 0;
            continue;
        }
        if b == b'%' {
            // Comment — skip to end of line
            while i < len && data[i] != b'\n' && data[i] != b'\r' {
                i += 1;
            }
            continue;
        }

        i += 1;
    }

    // Record state for any remaining text positions
    while next_tp_idx < sorted_positions.len() {
        let (orig_idx, _) = sorted_positions[next_tp_idx];
        results[orig_idx] = PrescanState {
            ctm: (ctm.a, ctm.b, ctm.c, ctm.d, ctm.e, ctm.f),
            font: current_font_idx.map(|idx| font_table[idx].clone()),
        };
        next_tp_idx += 1;
    }

    Some(results)
}
