use super::*;

/// Fast parser for a single operator inside a BT/ET text block.
///
/// Returns `Some((remaining_input, operator))` on success, `None` on failure
/// (caller should fall back to the generic `parse_operator_with_operands`).
pub(super) fn parse_text_operator_fast(input: &[u8]) -> Option<(&[u8], Operator)> {
    let mut pos = 0;
    // Small inline operand stack (max 8 operands for any PDF operator)
    let mut operands: [Option<FastOperand>; 8] = [None, None, None, None, None, None, None, None];
    let mut op_count: usize = 0;

    loop {
        // Skip whitespace
        while pos < input.len() && is_whitespace(input[pos]) {
            pos += 1;
        }
        if pos >= input.len() {
            return None;
        }

        let b = input[pos];
        match b {
            // Number operand
            b'0'..=b'9' | b'.' | b'+' | b'-' => {
                // Quick check: a lone '-' or '+' followed by non-digit is not a number
                if (b == b'-' || b == b'+')
                    && (pos + 1 >= input.len()
                        || (!input[pos + 1].is_ascii_digit() && input[pos + 1] != b'.'))
                {
                    return None; // fallback
                }
                if let Some((num, consumed)) = parse_float_fast(&input[pos..]) {
                    if op_count < 8 {
                        operands[op_count] = Some(FastOperand::Number(num));
                        op_count += 1;
                    }
                    pos += consumed;
                } else {
                    return None;
                }
            }
            // Literal string
            b'(' => {
                if let Some((bytes, end)) = parse_literal_string_fast(input, pos) {
                    if op_count < 8 {
                        operands[op_count] = Some(FastOperand::StringBytes(bytes));
                        op_count += 1;
                    }
                    pos = end;
                } else {
                    return None;
                }
            }
            // Hex string
            b'<' => {
                // Check it's not a dict <<
                if pos + 1 < input.len() && input[pos + 1] == b'<' {
                    return None; // dict — fall back to generic parser
                }
                if let Some((bytes, end)) = parse_hex_string_fast(input, pos) {
                    if op_count < 8 {
                        operands[op_count] = Some(FastOperand::StringBytes(bytes));
                        op_count += 1;
                    }
                    pos = end;
                } else {
                    return None;
                }
            }
            // Name
            b'/' => {
                let (name, end) = parse_name_fast(input, pos);
                if op_count < 8 {
                    operands[op_count] = Some(FastOperand::Name(name));
                    op_count += 1;
                }
                pos = end;
            }
            // Array (for TJ)
            b'[' => {
                if let Some((elements, end)) = parse_tj_array_fast(input, pos) {
                    if op_count < 8 {
                        operands[op_count] = Some(FastOperand::TextArray(elements));
                        op_count += 1;
                    }
                    pos = end;
                } else {
                    return None;
                }
            }
            // Operator name
            c if c.is_ascii_alphabetic() || c == b'\'' || c == b'"' || c == b'*' => {
                let op_start = pos;
                while pos < input.len()
                    && (input[pos].is_ascii_alphanumeric()
                        || input[pos] == b'\''
                        || input[pos] == b'"'
                        || input[pos] == b'*')
                {
                    pos += 1;
                }
                let op_bytes = &input[op_start..pos];
                let rest = &input[pos..];

                // Keywords that are operands, not operators
                if op_bytes == b"true" || op_bytes == b"false" || op_bytes == b"null" {
                    // These are operand values — skip them (rare in text blocks)
                    continue;
                }

                // Match operator and build typed variant
                let operator = match op_bytes {
                    b"ET" => Operator::EndText,
                    b"BT" => Operator::BeginText,
                    b"Tf" => {
                        let font = match &operands[0] {
                            Some(FastOperand::Name(n)) => n.clone(),
                            _ => String::new(),
                        };
                        let size = match &operands[1] {
                            Some(FastOperand::Number(n)) => *n,
                            // Font name might be in slot 0 and size in slot 1,
                            // but if only one operand, try it as the font name
                            _ => 12.0,
                        };
                        Operator::Tf { font, size }
                    }
                    b"Td" => {
                        let tx = match &operands[0] {
                            Some(FastOperand::Number(n)) => *n,
                            _ => 0.0,
                        };
                        let ty = match &operands[1] {
                            Some(FastOperand::Number(n)) => *n,
                            _ => 0.0,
                        };
                        Operator::Td { tx, ty }
                    }
                    b"TD" => {
                        let tx = match &operands[0] {
                            Some(FastOperand::Number(n)) => *n,
                            _ => 0.0,
                        };
                        let ty = match &operands[1] {
                            Some(FastOperand::Number(n)) => *n,
                            _ => 0.0,
                        };
                        Operator::TD { tx, ty }
                    }
                    b"Tm" => {
                        let get_n = |i: usize, def: f32| match &operands[i] {
                            Some(FastOperand::Number(n)) => *n,
                            _ => def,
                        };
                        Operator::Tm {
                            a: get_n(0, 1.0),
                            b: get_n(1, 0.0),
                            c: get_n(2, 0.0),
                            d: get_n(3, 1.0),
                            e: get_n(4, 0.0),
                            f: get_n(5, 0.0),
                        }
                    }
                    b"T*" => Operator::TStar,
                    b"Tj" => {
                        let text = match operands[0].take() {
                            Some(FastOperand::StringBytes(b)) => b,
                            _ => Vec::new(),
                        };
                        Operator::Tj { text }
                    }
                    b"TJ" => {
                        let array = match operands[0].take() {
                            Some(FastOperand::TextArray(a)) => a,
                            _ => Vec::new(),
                        };
                        Operator::TJ { array }
                    }
                    b"'" => {
                        let text = match operands[0].take() {
                            Some(FastOperand::StringBytes(b)) => b,
                            _ => Vec::new(),
                        };
                        Operator::Quote { text }
                    }
                    b"\"" => {
                        let word_space = match &operands[0] {
                            Some(FastOperand::Number(n)) => *n,
                            _ => 0.0,
                        };
                        let char_space = match &operands[1] {
                            Some(FastOperand::Number(n)) => *n,
                            _ => 0.0,
                        };
                        let text = match operands[2].take() {
                            Some(FastOperand::StringBytes(b)) => b,
                            _ => Vec::new(),
                        };
                        Operator::DoubleQuote {
                            word_space,
                            char_space,
                            text,
                        }
                    }
                    b"Tc" => {
                        let char_space = match &operands[0] {
                            Some(FastOperand::Number(n)) => *n,
                            _ => 0.0,
                        };
                        Operator::Tc { char_space }
                    }
                    b"Tw" => {
                        let word_space = match &operands[0] {
                            Some(FastOperand::Number(n)) => *n,
                            _ => 0.0,
                        };
                        Operator::Tw { word_space }
                    }
                    b"Tz" => {
                        let scale = match &operands[0] {
                            Some(FastOperand::Number(n)) => *n,
                            _ => 100.0,
                        };
                        Operator::Tz { scale }
                    }
                    b"TL" => {
                        let leading = match &operands[0] {
                            Some(FastOperand::Number(n)) => *n,
                            _ => 0.0,
                        };
                        Operator::TL { leading }
                    }
                    b"Tr" => {
                        let render = match &operands[0] {
                            Some(FastOperand::Number(n)) => *n as u8,
                            _ => 0,
                        };
                        Operator::Tr { render }
                    }
                    b"Ts" => {
                        let rise = match &operands[0] {
                            Some(FastOperand::Number(n)) => *n,
                            _ => 0.0,
                        };
                        Operator::Ts { rise }
                    }
                    b"q" => Operator::SaveState,
                    b"Q" => Operator::RestoreState,
                    b"cm" => {
                        let get_n = |i: usize, def: f32| match &operands[i] {
                            Some(FastOperand::Number(n)) => *n,
                            _ => def,
                        };
                        Operator::Cm {
                            a: get_n(0, 1.0),
                            b: get_n(1, 0.0),
                            c: get_n(2, 0.0),
                            d: get_n(3, 1.0),
                            e: get_n(4, 0.0),
                            f: get_n(5, 0.0),
                        }
                    }
                    b"rg" => {
                        let get_n = |i: usize| match &operands[i] {
                            Some(FastOperand::Number(n)) => *n,
                            _ => 0.0,
                        };
                        Operator::SetFillRgb {
                            r: get_n(0),
                            g: get_n(1),
                            b: get_n(2),
                        }
                    }
                    b"RG" => {
                        let get_n = |i: usize| match &operands[i] {
                            Some(FastOperand::Number(n)) => *n,
                            _ => 0.0,
                        };
                        Operator::SetStrokeRgb {
                            r: get_n(0),
                            g: get_n(1),
                            b: get_n(2),
                        }
                    }
                    b"g" => {
                        let gray = match &operands[0] {
                            Some(FastOperand::Number(n)) => *n,
                            _ => 0.0,
                        };
                        Operator::SetFillGray { gray }
                    }
                    b"G" => {
                        let gray = match &operands[0] {
                            Some(FastOperand::Number(n)) => *n,
                            _ => 0.0,
                        };
                        Operator::SetStrokeGray { gray }
                    }
                    b"k" => {
                        let get_n = |i: usize| match &operands[i] {
                            Some(FastOperand::Number(n)) => *n,
                            _ => 0.0,
                        };
                        Operator::SetFillCmyk {
                            c: get_n(0),
                            m: get_n(1),
                            y: get_n(2),
                            k: get_n(3),
                        }
                    }
                    b"K" => {
                        let get_n = |i: usize| match &operands[i] {
                            Some(FastOperand::Number(n)) => *n,
                            _ => 0.0,
                        };
                        Operator::SetStrokeCmyk {
                            c: get_n(0),
                            m: get_n(1),
                            y: get_n(2),
                            k: get_n(3),
                        }
                    }
                    b"cs" => {
                        let name = match &operands[0] {
                            Some(FastOperand::Name(n)) => n.clone(),
                            _ => "DeviceGray".to_string(),
                        };
                        Operator::SetFillColorSpace { name }
                    }
                    b"CS" => {
                        let name = match &operands[0] {
                            Some(FastOperand::Name(n)) => n.clone(),
                            _ => "DeviceGray".to_string(),
                        };
                        Operator::SetStrokeColorSpace { name }
                    }
                    b"sc" => {
                        let components: Vec<f32> = operands[..op_count]
                            .iter()
                            .filter_map(|o| match o {
                                Some(FastOperand::Number(n)) => Some(*n),
                                _ => None,
                            })
                            .collect();
                        Operator::SetFillColor { components }
                    }
                    b"SC" => {
                        let components: Vec<f32> = operands[..op_count]
                            .iter()
                            .filter_map(|o| match o {
                                Some(FastOperand::Number(n)) => Some(*n),
                                _ => None,
                            })
                            .collect();
                        Operator::SetStrokeColor { components }
                    }
                    b"scn" => {
                        let name = match &operands[op_count.saturating_sub(1)] {
                            Some(FastOperand::Name(n)) => Some(n.clone()),
                            _ => None,
                        };
                        let components: Vec<f32> = operands[..op_count]
                            .iter()
                            .filter_map(|o| match o {
                                Some(FastOperand::Number(n)) => Some(*n),
                                _ => None,
                            })
                            .collect();
                        Operator::SetFillColorN {
                            components,
                            name: name.map(Box::new),
                        }
                    }
                    b"SCN" => {
                        let name = match &operands[op_count.saturating_sub(1)] {
                            Some(FastOperand::Name(n)) => Some(n.clone()),
                            _ => None,
                        };
                        let components: Vec<f32> = operands[..op_count]
                            .iter()
                            .filter_map(|o| match o {
                                Some(FastOperand::Number(n)) => Some(*n),
                                _ => None,
                            })
                            .collect();
                        Operator::SetStrokeColorN {
                            components,
                            name: name.map(Box::new),
                        }
                    }
                    b"gs" => {
                        let dict_name = match &operands[0] {
                            Some(FastOperand::Name(n)) => n.clone(),
                            _ => String::new(),
                        };
                        Operator::SetExtGState { dict_name }
                    }
                    b"Do" => {
                        // See the nom-parser "Do" arm in `build_operator` for why
                        // this reads the last operand, not operands[0].
                        let name = match &operands[op_count.saturating_sub(1)] {
                            Some(FastOperand::Name(n)) => n.clone(),
                            _ => String::new(),
                        };
                        Operator::Do { name }
                    }
                    b"w" => {
                        let width = match &operands[0] {
                            Some(FastOperand::Number(n)) => *n,
                            _ => 1.0,
                        };
                        Operator::SetLineWidth { width }
                    }
                    b"J" => {
                        let cap_style = match &operands[0] {
                            Some(FastOperand::Number(n)) => *n as u8,
                            _ => 0,
                        };
                        Operator::SetLineCap { cap_style }
                    }
                    b"j" => {
                        let join_style = match &operands[0] {
                            Some(FastOperand::Number(n)) => *n as u8,
                            _ => 0,
                        };
                        Operator::SetLineJoin { join_style }
                    }
                    b"i" => {
                        let tolerance = match &operands[0] {
                            Some(FastOperand::Number(n)) => *n,
                            _ => 0.0,
                        };
                        Operator::SetFlatness { tolerance }
                    }
                    _ => {
                        // Unknown operator inside BT/ET — fall back to generic parser
                        return None;
                    }
                };

                return Some((rest, operator));
            }
            _ => {
                // Unknown byte — fall back to generic parser
                return None;
            }
        }
    }
}

// Byte classification for fast graphics scanning.
// 0 = skip (whitespace, digits, dot, sign) — bulk-skippable
// 1 = alpha/quote/star — operator start
// 2 = '(' — literal string start
// 3 = '<' — hex string or dict start
// 4 = '[' — array start
// 5 = '/' — name start
// 6 = '%' — comment start
// 7 = other (unknown byte)
