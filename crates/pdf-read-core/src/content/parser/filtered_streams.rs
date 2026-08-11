use super::*;

pub(super) fn parse_content_stream_paths_only_uncounted(data: &[u8]) -> Result<Vec<Operator>> {
    let estimated_capacity = data.len() / 20;
    let mut operators = Vec::with_capacity(estimated_capacity.min(100_000));
    let len = data.len();
    let mut i: usize = 0;
    let mut operand_start: usize = 0;
    let mut consecutive_errors: usize = 0;

    loop {
        // Bulk-skip whitespace, digits, dots, signs
        while i < len && BYTE_CLASS[data[i] as usize] == SCAN_SKIP {
            i += 1;
        }
        if i >= len {
            break;
        }
        check_operator_budget(operators.len())?;
        if operators.len() >= effective_max_operators() {
            push_operator_cap_warning();
            break;
        }

        match BYTE_CLASS[data[i] as usize] {
            SCAN_ALPHA => {
                let first_byte = data[i];
                let second_is_non_alpha =
                    i + 1 >= len || BYTE_CLASS[data[i + 1] as usize] != SCAN_ALPHA;

                // Fast path: zero-operand single-char operators
                if second_is_non_alpha {
                    let operands = &data[operand_start..i];
                    let op = match first_byte {
                        // Path painting (zero operands)
                        b'S' => Some(Operator::Stroke),
                        b'n' => Some(Operator::EndPath),
                        b'h' => Some(Operator::ClosePath),
                        // Graphics state (zero operands)
                        b'q' => Some(Operator::SaveState),
                        b'Q' => Some(Operator::RestoreState),
                        // Path construction (numeric operands)
                        b'm' => parse_floats::<2>(operands)
                            .map(|f| Operator::MoveTo { x: f[0], y: f[1] }),
                        b'l' => parse_floats::<2>(operands)
                            .map(|f| Operator::LineTo { x: f[0], y: f[1] }),
                        b'c' => parse_floats::<6>(operands).map(|f| Operator::CurveTo {
                            x1: f[0],
                            y1: f[1],
                            x2: f[2],
                            y2: f[3],
                            x3: f[4],
                            y3: f[5],
                        }),
                        b'v' => parse_floats::<4>(operands).map(|f| Operator::CurveToV {
                            x2: f[0],
                            y2: f[1],
                            x3: f[2],
                            y3: f[3],
                        }),
                        b'y' => parse_floats::<4>(operands).map(|f| Operator::CurveToY {
                            x1: f[0],
                            y1: f[1],
                            x3: f[2],
                            y3: f[3],
                        }),
                        // Line style (single numeric operand)
                        b'w' => parse_floats::<1>(operands)
                            .map(|f| Operator::SetLineWidth { width: f[0] }),
                        b'J' => parse_floats::<1>(operands).map(|f| Operator::SetLineCap {
                            cap_style: f[0] as u8,
                        }),
                        b'j' => parse_floats::<1>(operands).map(|f| Operator::SetLineJoin {
                            join_style: f[0] as u8,
                        }),
                        b'M' => parse_floats::<1>(operands)
                            .map(|f| Operator::SetMiterLimit { limit: f[0] }),
                        // Color (single float)
                        b'g' => parse_floats::<1>(operands)
                            .map(|f| Operator::SetFillGray { gray: f[0] }),
                        b'G' => parse_floats::<1>(operands)
                            .map(|f| Operator::SetStrokeGray { gray: f[0] }),
                        // Fill (zero operands) — f and F
                        b'f' | b'F' => Some(Operator::Fill),
                        // Fill+stroke (zero operands)
                        b'B' => Some(Operator::FillStroke),
                        b'b' => Some(Operator::CloseFillStroke),
                        // s = close path + stroke (not a named variant, emit ClosePath + Stroke)
                        b's' => {
                            operators.push(Operator::ClosePath);
                            Some(Operator::Stroke)
                        }
                        // Clip (zero operands)
                        b'W' => Some(Operator::ClipNonZero),
                        // Flatness
                        b'i' => {
                            operand_start = i + 1;
                            i += 1;
                            consecutive_errors = 0;
                            continue;
                        }
                        _ => None,
                    };
                    if let Some(op) = op {
                        operators.push(op);
                        i += 1;
                        operand_start = i;
                        consecutive_errors = 0;
                        continue;
                    }
                }

                // Multi-char operator: read full name
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

                // Keyword operands
                if op == b"true" || op == b"false" || op == b"null" {
                    consecutive_errors = 0;
                    continue;
                }

                consecutive_errors = 0;
                let operands = &data[operand_start..op_start];

                // Skip BT/ET text blocks
                if op == b"BT" {
                    match scan_to_et(&data[i..]) {
                        Some(rest) => {
                            i = len - rest.len();
                            operand_start = i;
                        }
                        None => break,
                    }
                    continue;
                }

                // Fast-path multi-char operators with numeric operands
                let fast_op =
                    match op {
                        b"cm" => parse_six_floats(operands)
                            .map(|(a, b, c, d, e, f)| Operator::Cm { a, b, c, d, e, f }),
                        b"re" => parse_floats::<4>(operands).map(|f| Operator::Rectangle {
                            x: f[0],
                            y: f[1],
                            width: f[2],
                            height: f[3],
                        }),
                        b"rg" => parse_floats::<3>(operands).map(|f| Operator::SetFillRgb {
                            r: f[0],
                            g: f[1],
                            b: f[2],
                        }),
                        b"RG" => parse_floats::<3>(operands).map(|f| Operator::SetStrokeRgb {
                            r: f[0],
                            g: f[1],
                            b: f[2],
                        }),
                        b"k" => parse_floats::<4>(operands).map(|f| Operator::SetFillCmyk {
                            c: f[0],
                            m: f[1],
                            y: f[2],
                            k: f[3],
                        }),
                        b"K" => parse_floats::<4>(operands).map(|f| Operator::SetStrokeCmyk {
                            c: f[0],
                            m: f[1],
                            y: f[2],
                            k: f[3],
                        }),
                        b"f*" => Some(Operator::FillEvenOdd),
                        b"B*" => Some(Operator::FillStrokeEvenOdd),
                        b"b*" => Some(Operator::CloseFillStrokeEvenOdd),
                        b"W*" => Some(Operator::ClipEvenOdd),
                        // Skip text/color-space/shading operators that don't affect paths
                        b"ET" | b"Tc" | b"Tw" | b"Tz" | b"TL" | b"Tf" | b"Tr" | b"Ts" | b"Td"
                        | b"TD" | b"Tm" | b"Tj" | b"TJ" | b"T*" | b"cs" | b"CS" | b"sc" | b"SC"
                        | b"scn" | b"SCN" | b"ri" | b"sh" | b"EI" => {
                            operand_start = i;
                            continue;
                        }
                        _ => None,
                    };

                if let Some(op) = fast_op {
                    operators.push(op);
                    operand_start = i;
                    continue;
                }

                // Slow path: fall back to full nom parser for complex operators
                // (Do, gs, d, BI/ID/EI, etc.)
                match parse_operator_with_operands(&data[operand_start..]) {
                    Ok((rest, op)) => {
                        operators.push(op);
                        i = len - rest.len();
                        operand_start = i;
                    }
                    Err(_) => {
                        operand_start = i;
                    }
                }
            }

            SCAN_PAREN => match skip_literal_string_raw(data, i) {
                Some(end) => {
                    i = end;
                    consecutive_errors = 0;
                }
                None => {
                    i += 1;
                    consecutive_errors += 1;
                }
            },
            SCAN_ANGLE => {
                if i + 1 < len && data[i + 1] == b'<' {
                    // Dictionary — skip (shouldn't appear in content streams)
                    i += 2;
                } else {
                    match skip_hex_string_raw(data, i) {
                        Some(end) => {
                            i = end;
                            consecutive_errors = 0;
                        }
                        None => {
                            i += 1;
                            consecutive_errors += 1;
                        }
                    }
                }
            }
            SCAN_BRACKET => {
                // Array — skip to matching ']'
                i += 1;
                let mut depth = 1u32;
                while i < len && depth > 0 {
                    match data[i] {
                        b'[' => depth += 1,
                        b']' => depth -= 1,
                        b'(' => {
                            if let Some(end) = skip_literal_string_raw(data, i) {
                                i = end;
                                continue;
                            }
                        }
                        _ => {}
                    }
                    i += 1;
                }
            }
            SCAN_SLASH => {
                // Name token — skip
                i = skip_name_raw(data, i);
            }
            SCAN_PERCENT => {
                // Comment — skip to end of line
                while i < len && data[i] != b'\n' && data[i] != b'\r' {
                    i += 1;
                }
            }
            _ => {
                i += 1;
                consecutive_errors += 1;
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    log::warn!(
                        "Content stream had {} consecutive parse errors, bailing out ({} bytes remaining)",
                        MAX_CONSECUTIVE_ERRORS,
                        len - i
                    );
                    break;
                }
            }
        }
    }

    Ok(operators)
}

pub(super) fn parse_content_stream_text_only_uncounted(data: &[u8]) -> Result<Vec<Operator>> {
    let estimated_capacity = data.len() / 40;
    let mut operators = Vec::with_capacity(estimated_capacity.min(50_000));
    let mut input = data;
    let mut consecutive_errors: usize = 0;
    let mut inside_text = false;

    while !input.is_empty() {
        if let Ok((rest, _)) = multispace0::<&[u8], nom::error::Error<&[u8]>>.parse(input) {
            input = rest;
        }
        if input.is_empty() {
            break;
        }

        check_operator_budget(operators.len())?;

        if operators.len() >= effective_max_operators() {
            push_operator_cap_warning();
            break;
        }

        if inside_text {
            // Inside BT/ET: full parse, identical to parse_content_stream
            match parse_operator_with_operands(input) {
                Ok((rest, op)) => {
                    if matches!(op, Operator::EndText) {
                        inside_text = false;
                    }
                    operators.push(op);
                    input = rest;
                    consecutive_errors = 0;
                }
                Err(_) => {
                    consecutive_errors += 1;
                    if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                        log::warn!(
                            "Content stream had {} consecutive parse errors, bailing out ({} bytes remaining)",
                            MAX_CONSECUTIVE_ERRORS,
                            input.len()
                        );
                        break;
                    }
                    if input.len() > 1 {
                        input = &input[1..];
                    } else {
                        break;
                    }
                }
            }
        } else {
            // Outside BT/ET: byte-level scan — skip operands and graphics
            // operators using raw index arithmetic (no nom IResult overhead).
            match scan_graphics_region(input, &mut consecutive_errors) {
                ScanResult::EndOfData => break,
                ScanResult::FoundBT { rest } => {
                    operators.push(Operator::BeginText);
                    input = rest;
                    inside_text = true;
                }
                ScanResult::InlineImage { rest } => match parse_inline_image(rest) {
                    Ok((rest2, _)) => input = rest2,
                    Err(_) => input = rest,
                },
                ScanResult::NeedFullParse {
                    operand_start,
                    after_op,
                } => match parse_operator_with_operands(operand_start) {
                    Ok((rest2, op)) => {
                        operators.push(op);
                        input = rest2;
                    }
                    Err(_) => input = after_op,
                },
                ScanResult::DeferredThenText {
                    deferred_start,
                    trigger_start,
                } => {
                    // Re-parse the deferred q/cm/Q region to emit CTM-affecting ops.
                    // The trigger (BT/BI/Do/etc.) is NOT included — the next iteration
                    // of the outer loop re-enters scan_graphics_region which returns
                    // the trigger via FoundBT / InlineImage / NeedFullParse.
                    let mut remaining = deferred_start;
                    while remaining.len() > trigger_start.len() {
                        match parse_operator_with_operands(remaining) {
                            Ok((rest2, op)) => {
                                operators.push(op);
                                remaining = rest2;
                            }
                            Err(_) => {
                                if remaining.len() > 1 {
                                    remaining = &remaining[1..];
                                } else {
                                    break;
                                }
                            }
                        }
                    }
                    input = trigger_start;
                    consecutive_errors = 0;
                }
                ScanResult::SimpleOp { op, rest } => {
                    operators.push(op);
                    input = rest;
                }
                ScanResult::TooManyErrors { remaining } => {
                    log::warn!(
                        "Content stream had {} consecutive parse errors, bailing out ({} bytes remaining)",
                        MAX_CONSECUTIVE_ERRORS,
                        remaining.len()
                    );
                    break;
                }
            }
        }
    }

    Ok(operators)
}

pub(super) fn parse_content_stream_images_only_uncounted(data: &[u8]) -> Result<Vec<Operator>> {
    let mut operators = Vec::with_capacity(256);
    let mut input = data;
    let mut consecutive_errors: usize = 0;
    let mut inside_text = false;

    while !input.is_empty() {
        if let Ok((rest, _)) = multispace0::<&[u8], nom::error::Error<&[u8]>>.parse(input) {
            input = rest;
        }
        if input.is_empty() {
            break;
        }

        check_operator_budget(operators.len())?;

        if operators.len() >= effective_max_operators() {
            break;
        }

        if inside_text {
            // Inside BT/ET: skip everything until ET
            match scan_to_et(input) {
                Some(rest) => {
                    input = rest;
                    inside_text = false;
                    consecutive_errors = 0;
                }
                None => break, // No ET found, end of stream
            }
        } else {
            // Outside BT/ET: use scan_graphics_region but handle differently
            match scan_graphics_region(input, &mut consecutive_errors) {
                ScanResult::EndOfData => break,
                ScanResult::FoundBT { rest } => {
                    // Skip the text block instead of parsing it
                    input = rest;
                    inside_text = true;
                }
                ScanResult::InlineImage { rest } => match parse_inline_image(rest) {
                    Ok((rest2, op)) => {
                        operators.push(op);
                        input = rest2;
                    }
                    Err(_) => input = rest,
                },
                ScanResult::NeedFullParse {
                    operand_start,
                    after_op,
                } => match parse_operator_with_operands(operand_start) {
                    Ok((rest2, op)) => {
                        operators.push(op);
                        input = rest2;
                    }
                    Err(_) => input = after_op,
                },
                ScanResult::DeferredThenText {
                    deferred_start,
                    trigger_start,
                } => {
                    let mut remaining = deferred_start;
                    while remaining.len() > trigger_start.len() {
                        match parse_operator_with_operands(remaining) {
                            Ok((rest2, op)) => {
                                operators.push(op);
                                remaining = rest2;
                            }
                            Err(_) => {
                                if remaining.len() > 1 {
                                    remaining = &remaining[1..];
                                } else {
                                    break;
                                }
                            }
                        }
                    }
                    input = trigger_start;
                    consecutive_errors = 0;
                }
                ScanResult::SimpleOp { op, rest } => {
                    operators.push(op);
                    input = rest;
                }
                ScanResult::TooManyErrors { .. } => break,
            }
        }
    }

    Ok(operators)
}

/// Skip forward until we find the ET operator (end text).
/// Returns the remaining input after ET, or None if not found.
pub(super) fn scan_to_et(data: &[u8]) -> Option<&[u8]> {
    let mut i = 0;
    while i + 1 < data.len() {
        if data[i] == b'E' && data[i + 1] == b'T' {
            // Verify it's a real ET operator (not part of a string)
            let before_ok = i == 0
                || data[i - 1].is_ascii_whitespace()
                || data[i - 1] == b')'
                || data[i - 1] == b'>';
            let after_ok =
                i + 2 >= data.len() || data[i + 2].is_ascii_whitespace() || data[i + 2] == b'%';
            if before_ok && after_ok {
                return Some(&data[i + 2..]);
            }
        }
        // Skip strings to avoid false matches inside text
        if data[i] == b'(' {
            i += 1;
            let mut depth = 1;
            while i < data.len() && depth > 0 {
                match data[i] {
                    b'(' => depth += 1,
                    b')' => depth -= 1,
                    b'\\' => i += 1, // skip escaped char
                    _ => {}
                }
                i += 1;
            }
            continue;
        }
        if data[i] == b'<' && (i + 1 >= data.len() || data[i + 1] != b'<') {
            i += 1;
            while i < data.len() && data[i] != b'>' {
                i += 1;
            }
            if i < data.len() {
                i += 1;
            }
            continue;
        }
        i += 1;
    }
    None
}
