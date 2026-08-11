use super::*;

/// Parse a sub-region of a content stream for text operators.
/// Used by the pre-scan path to parse only identified text-bearing regions.
pub(super) fn parse_region_text_only<F>(data: &[u8], handler: &mut F) -> Result<()>
where
    F: FnMut(Operator) -> Result<()>,
{
    let mut input = data;
    let mut consecutive_errors: usize = 0;
    let mut inside_text = false;
    let mut op_count: usize = 0;

    while !input.is_empty() {
        while !input.is_empty() && input[0].is_ascii_whitespace() {
            input = &input[1..];
        }
        if input.is_empty() {
            break;
        }

        if op_count >= effective_max_operators() {
            break;
        }

        if inside_text {
            if let Some((rest, op)) = parse_text_operator_fast(input) {
                if matches!(op, Operator::EndText) {
                    inside_text = false;
                }
                handler(op)?;
                op_count += 1;
                input = rest;
                consecutive_errors = 0;
            } else {
                match parse_operator_with_operands(input) {
                    Ok((rest, op)) => {
                        if matches!(op, Operator::EndText) {
                            inside_text = false;
                        }
                        handler(op)?;
                        op_count += 1;
                        input = rest;
                        consecutive_errors = 0;
                    }
                    Err(_) => {
                        consecutive_errors += 1;
                        if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                            break;
                        }
                        if input.len() > 1 {
                            input = &input[1..];
                        } else {
                            break;
                        }
                    }
                }
            }
        } else {
            match scan_graphics_region(input, &mut consecutive_errors) {
                ScanResult::EndOfData => break,
                ScanResult::FoundBT { rest } => {
                    handler(Operator::BeginText)?;
                    op_count += 1;
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
                        handler(op)?;
                        op_count += 1;
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
                                handler(op)?;
                                op_count += 1;
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
                    handler(op)?;
                    op_count += 1;
                    input = rest;
                }
                ScanResult::TooManyErrors { .. } => break,
            }
        }
    }

    Ok(())
}
