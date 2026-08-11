//! Content stream parser.
//!
//! This module parses PDF content streams into a sequence of operators.
//! Content streams are fundamentally different from the main PDF structure:
//! they use a postfix notation where operands come before operators.
//!
//! Example content stream:
//! ```text
//! BT
//!   /F1 12 Tf
//!   100 700 Td
//!   (Hello, World!) Tj
//! ET
//! ```

use crate::content::operators::{Operator, TextElement};
use crate::error::Result;
use crate::object::Object;
use crate::parser::parse_object;
use nom::bytes::complete::take_while1;
use nom::character::complete::multispace0;
use nom::IResult;
use nom::Parser;
use smallvec::SmallVec;
use std::collections::HashMap;

/// Default maximum number of operators to parse from a single content
/// stream. Prevents pathological inputs (e.g., Isartor 6.1.12) from
/// consuming unbounded time and memory.
///
/// Callers can override via [`set_max_ops_per_stream`] to raise the
/// cap (or set `usize::MAX` for effectively unbounded — use with
/// caution on adversarial PDFs).
const MAX_OPERATORS: usize = 1_000_000;

/// Global cap override for content-stream operator count. `0`
/// means "use [`MAX_OPERATORS`] default"; any other value is the
/// effective cap. Atomic so it's safe to set from one thread while
/// extraction runs on another (e.g. parallel-page extraction).
static MAX_OPERATORS_OVERRIDE: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Set the global content-stream operator cap. `None` keeps the
/// default of `MAX_OPERATORS` (1,000,000). `Some(n)` overrides to `n`
/// — pass `Some(usize::MAX)` for effectively unbounded.
///
/// Returns the previous override (or `None` if the default was active).
/// The override is process-global; setting it on one thread affects all
/// concurrent extractions.
///
/// **Use case**: large technical PDFs (textbooks, ISO standards) that
/// have legitimate content streams exceeding 1,000,000 operators. The
/// default cap exists to bound the cost of adversarial inputs; raise
/// it when you know the inputs are trusted.
pub fn set_max_ops_per_stream(limit: Option<usize>) -> Option<usize> {
    let new = limit.unwrap_or(0);
    let prev = MAX_OPERATORS_OVERRIDE.swap(new, std::sync::atomic::Ordering::SeqCst);
    if prev == 0 {
        None
    } else {
        Some(prev)
    }
}

/// Current effective operator cap. Reads the override if set; otherwise
/// returns [`MAX_OPERATORS`]. Internal hot-path helper.
#[inline]
fn effective_max_operators() -> usize {
    let override_val = MAX_OPERATORS_OVERRIDE.load(std::sync::atomic::Ordering::Relaxed);
    if override_val == 0 {
        MAX_OPERATORS
    } else {
        override_val
    }
}

/// Maximum consecutive parse errors (byte skips) before bailing out.
///
/// If we skip this many bytes without finding a valid operator, the
/// remaining data is likely junk, not a parseable content stream.
const MAX_CONSECUTIVE_ERRORS: usize = 1024;

/// Refuse a stream that has parsed past what the call reserved for one operator vector.
///
/// Deliberately not folded into the truncation cap below it. That cap keeps what it has
/// and warns, which is right for an implementation limit and wrong for a budget: a page
/// silently shortened is indistinguishable from a page that was that short. Checked on a
/// stride so the cost stays off the per-operator path.
#[inline]
fn check_operator_budget(count: usize) -> Result<()> {
    const STRIDE: usize = 4096;
    if count % STRIDE == 0 {
        crate::budget::check_stream_operators(count)?;
    }
    Ok(())
}

/// Emit the operator-cap-exceeded warning at the actual *effective* cap
/// (which may have been overridden via `set_max_ops_per_stream`). PDF
/// Spec Annex C documents implementation limits; the cap exists to
/// bound parser cost on adversarial inputs.
#[inline]
fn push_operator_cap_warning() {
    let cap = effective_max_operators();
    let msg = format!("Content stream exceeded {cap} operators, truncating");
    log::warn!("{msg}");
    crate::extractors::warnings::push_global_warning(crate::extractors::warnings::Warning {
        category: crate::extractors::warnings::WarningCategory::OperatorCapExceeded,
        page: None,
        message: msg,
        spec_section: Some("Annex C"),
    });
}

/// Parse a content stream into a sequence of operators.
///
/// Content streams use postfix notation where operands precede the operator.
/// For example: `100 200 Td` means "move text position to (100, 200)".
///
/// Includes safety limits: bails out after `MAX_OPERATORS` operators or
/// `MAX_CONSECUTIVE_ERRORS` consecutive parse failures.
pub fn parse_content_stream(data: &[u8]) -> Result<Vec<Operator>> {
    let operators = parse_content_stream_uncounted(data)?;
    crate::metrics::record_content_operators(operators.len());
    crate::budget::check_operator_growth(operators.len())?;
    Ok(operators)
}

fn parse_content_stream_uncounted(data: &[u8]) -> Result<Vec<Operator>> {
    let estimated_capacity = data.len() / 20;
    let mut operators = Vec::with_capacity(estimated_capacity.min(100_000));
    let mut input = data;
    let mut consecutive_errors: usize = 0;

    // Parse until we consume all input
    while !input.is_empty() {
        // Skip whitespace
        if let Ok((rest, _)) = multispace0::<&[u8], nom::error::Error<&[u8]>>.parse(input) {
            input = rest;
        }

        // Check if we're done
        if input.is_empty() {
            break;
        }

        // Parse one operator with its operands
        match parse_operator_with_operands(input) {
            Ok((rest, op)) => {
                operators.push(op);
                input = rest;
                consecutive_errors = 0;

                check_operator_budget(operators.len())?;

                if operators.len() >= effective_max_operators() {
                    push_operator_cap_warning();
                    break;
                }
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
                // If we can't parse, skip the problematic byte and continue
                // This makes us more resilient to malformed streams
                if input.len() > 1 {
                    input = &input[1..];
                } else {
                    break;
                }
            }
        }
    }

    Ok(operators)
}

/// Parse a content stream for path extraction, skipping BT/ET text blocks.
///
/// This is a performance-optimized variant of [`parse_content_stream`] that:
/// 1. Skips text object blocks (BT…ET) at the byte level
/// 2. Parses common path/state/color operators at the byte level using fast
///    float parsing — no nom overhead, no Object heap allocation
/// 3. Falls back to full `parse_operator_with_operands` only for complex
///    operators (Do, gs, d, inline images)
///
/// # Performance
///
/// For graphics-heavy pages (e.g., 15 MB of vector paths with 50K cm operators),
/// this is 2–4× faster than the full parser because it avoids allocating
/// `Object::Real` for each numeric operand. Common operators like `m`, `l`, `c`,
/// `re`, `cm`, `rg` are parsed entirely from raw bytes.
pub fn parse_content_stream_paths_only(data: &[u8]) -> Result<Vec<Operator>> {
    let operators = parse_content_stream_paths_only_uncounted(data)?;
    crate::metrics::record_content_operators(operators.len());
    crate::budget::check_operator_growth(operators.len())?;
    Ok(operators)
}

fn parse_content_stream_paths_only_uncounted(data: &[u8]) -> Result<Vec<Operator>> {
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

/// Parse a content stream for text extraction, skipping pure graphics operators.
///
/// This is a performance-optimized variant of [`parse_content_stream`] that
/// avoids constructing `Object` operands for operators that only affect paths,
/// clipping, and non-text graphics state. Inside BT/ET text blocks, parsing is
/// identical to the full parser.
///
/// # Performance
///
/// For graphics-heavy pages (e.g., 1–12 MB of path data), this can be 3–5x
/// faster than full parsing while producing identical text extraction results.
/// The speedup comes from byte-level operand skipping (no `f64` parsing, no
/// heap allocation) and discarding path/clipping operators entirely.
///
/// # Safety limits
///
/// Same as [`parse_content_stream`]: bails out after `MAX_OPERATORS`
/// operators or `MAX_CONSECUTIVE_ERRORS` consecutive parse failures.
pub fn parse_content_stream_text_only(data: &[u8]) -> Result<Vec<Operator>> {
    let operators = parse_content_stream_text_only_uncounted(data)?;
    crate::metrics::record_content_operators(operators.len());
    crate::budget::check_operator_growth(operators.len())?;
    Ok(operators)
}

fn parse_content_stream_text_only_uncounted(data: &[u8]) -> Result<Vec<Operator>> {
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

/// Graphics state snapshot captured at a text position by [`forward_scan_ctm`].
#[derive(Debug)]
struct PrescanState {
    /// Accumulated CTM components (a, b, c, d, e, f).
    ctm: (f32, f32, f32, f32, f32, f32),
    /// Current font name and size from the most recent `Tf` operator, if any.
    font: Option<(String, f32)>,
}

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
fn forward_scan_ctm(data: &[u8], text_positions: &[usize]) -> Option<Vec<PrescanState>> {
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

/// Result of pre-scanning a content stream for text regions.
#[derive(Debug)]
enum PrescanResult {
    /// No text operators in the stream.
    Empty,
    /// Text regions found with complete CTM context from backward scan alone.
    Regions(Vec<(usize, usize)>),
    /// Text regions with graphics state from a forward CTM scan.
    ///
    /// Used when the backward scan hit the 4KB limit for at least one BT,
    /// meaning outer CTM context may be missing. Each region is paired with
    /// the full graphics state at its BT/Do position.
    RegionsWithCtm {
        regions: Vec<(usize, usize)>,
        /// One entry per region, in the same order as `regions`.
        region_states: Vec<PrescanState>,
    },
}

impl PrescanResult {
    /// Get the regions from any variant (for testing).
    #[cfg(test)]
    fn regions(&self) -> &[(usize, usize)] {
        match self {
            PrescanResult::Empty => &[],
            PrescanResult::Regions(r) => r,
            PrescanResult::RegionsWithCtm { regions, .. } => regions,
        }
    }
}

/// SIMD-accelerated pre-scan to identify text-bearing regions in a content stream.
///
/// Finds BT/Do operators via memchr, then for each one determines the region
/// boundaries and required graphics state. When the backward scan can capture
/// all enclosing `q`/`cm` context within 4KB, returns [`PrescanResult::Regions`].
/// Otherwise, runs a lightweight forward CTM scan to capture the full graphics
/// state and returns [`PrescanResult::RegionsWithCtm`].
///
/// # Arguments
///
/// * `data` - Raw content stream bytes
///
/// # Returns
///
/// Returns `None` if the forward scan fails, signaling the caller to fall back
/// to full stream parsing.
fn prescan_text_regions(data: &[u8]) -> Option<PrescanResult> {
    fn is_boundary(b: u8) -> bool {
        b.is_ascii_whitespace()
            || matches!(
                b,
                b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
            )
    }

    let len = data.len();
    // Collect positions of BT and Do operators (text-bearing operators)
    let mut text_positions: Vec<usize> = Vec::new();
    let mut offset = 0;

    // Use memchr to find 'B' and 'D' candidates (SIMD-accelerated)
    loop {
        match memchr::memchr2(b'B', b'D', &data[offset..]) {
            None => break,
            Some(rel_pos) => {
                let pos = offset + rel_pos;
                offset = pos + 1;

                // Check for "BT" at boundary
                #[allow(clippy::if_same_then_else)]
                if data[pos] == b'B' && pos + 1 < len && data[pos + 1] == b'T' {
                    let before_ok = pos == 0 || is_boundary(data[pos - 1]);
                    let after_ok = pos + 2 >= len || is_boundary(data[pos + 2]);
                    if before_ok && after_ok {
                        text_positions.push(pos);
                    }
                }
                // Check for "Do" at boundary
                else if data[pos] == b'D' && pos + 1 < len && data[pos + 1] == b'o' {
                    let before_ok = pos == 0 || is_boundary(data[pos - 1]);
                    let after_ok = pos + 2 >= len || is_boundary(data[pos + 2]);
                    if before_ok && after_ok {
                        text_positions.push(pos);
                    }
                }
            }
        }
    }

    if text_positions.is_empty() {
        return Some(PrescanResult::Empty);
    }

    // Drop Do positions when Do dominates BT (chart/figure graphics that
    // would merge prescan regions across the entire stream).
    // Everything below materialises one region per text position, and on the CTM path a
    // graphics state with an owned font name beside it, so the prescan's own footprint is
    // proportional to attacker-controlled input. Declining leaves the caller on the full
    // parser, which is the bounded path: it refuses on the operator budget instead.
    if text_positions.len() > crate::budget::prescan_region_ceiling() {
        return None;
    }

    let bt_count = text_positions
        .iter()
        .filter(|&&p| p + 1 < len && data[p] == b'B')
        .count();
    let do_count = text_positions.len() - bt_count;
    if do_count > 50 && do_count > bt_count * 10 {
        text_positions.retain(|&p| p + 1 < len && data[p] == b'B');
        if text_positions.is_empty() {
            return Some(PrescanResult::Empty);
        }
    }

    // For each text position, scan backwards to find the nearest unmatched 'q'
    // to capture CTM state (cm operators between q and BT/Do).
    let mut regions: Vec<(usize, usize)> = Vec::new();
    let mut needs_forward_ctm = false;

    for &tp in &text_positions {
        // Find region start: scan backwards for unmatched q
        let (region_start, hit_limit) = find_region_start(data, tp);

        if hit_limit {
            needs_forward_ctm = true;
        }

        // Find region end: for BT, find matching ET; for Do, end after "Do"
        let region_end = if data[tp] == b'B' {
            // Find matching ET
            find_matching_et(data, tp + 2).unwrap_or(len)
        } else {
            // Do operator: include operands before and the operator itself
            tp + 2
        };

        let end = region_end.min(len);
        regions.push((region_start, end));
    }

    // Merge overlapping/adjacent regions
    if regions.is_empty() {
        return Some(PrescanResult::Empty);
    }

    if needs_forward_ctm {
        // At least one BT was too far from the start of the stream for the
        // backward scan to capture all enclosing CTM context. Run a lightweight
        // forward scan to get the full graphics state at each BT/Do position.
        //
        // Regions start at the BT/Do position itself (not the backward-scanned
        // q) to avoid q/Q nesting issues with the SaveState/RestoreState
        // wrapping. The forward scan also tracks font state so BT blocks that
        // inherit fonts from prior state get the correct Tf injected.
        let states = forward_scan_ctm(data, &text_positions)?;

        // Build BT-based regions with their graphics state.
        // Extend each region to include preceding BDC/BMC and following EMC
        // so that marked-content operators are preserved in tagged PDFs.
        let mut ctm_regions: Vec<(usize, usize)> = Vec::new();
        for &tp in &text_positions {
            let region_start = find_preceding_marked_content(data, tp);
            let region_end = if data[tp] == b'B' {
                let et_end = find_matching_et(data, tp + 2).unwrap_or(len);
                find_following_emc(data, et_end)
            } else {
                tp + 2
            };
            ctm_regions.push((region_start, region_end.min(len)));
        }

        // Merge overlapping regions and track which state goes with each.
        let mut indexed: Vec<((usize, usize), PrescanState)> =
            ctm_regions.into_iter().zip(states).collect();
        indexed.sort_by_key(|&(r, _)| r.0);

        let mut merged: Vec<(usize, usize)> = Vec::new();
        let mut merged_states: Vec<PrescanState> = Vec::new();

        for (r, state) in indexed {
            if let Some(last) = merged.last_mut() {
                if r.0 <= last.1 {
                    last.1 = last.1.max(r.1);
                    continue; // Merged — keep the state from the first region
                }
            }
            merged.push(r);
            merged_states.push(state);
        }

        return Some(PrescanResult::RegionsWithCtm {
            regions: merged,
            region_states: merged_states,
        });
    }

    regions.sort_unstable_by_key(|r| r.0);
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for r in regions {
        if let Some(last) = merged.last_mut() {
            if r.0 <= last.1 {
                last.1 = last.1.max(r.1);
                continue;
            }
        }
        merged.push(r);
    }

    Some(PrescanResult::Regions(merged))
}

/// Scan backwards from `pos` to find the start of the graphics state context.
///
/// Looks for the nearest unmatched `q` operator within a 4KB window,
/// handling nested `q`/`Q` pairs.
///
/// # Arguments
///
/// * `data` - Full content stream bytes
/// * `pos` - Byte offset to scan backwards from (typically a BT/Do position)
///
/// # Returns
///
/// `(offset, hit_limit)` where `offset` is the position of the nearest
/// unmatched `q` (or `pos` if none found), and `hit_limit` is true if the
/// 4KB scan window didn't reach the beginning of the data. When `hit_limit`
/// is true, there may be additional enclosing `q`/`cm` operators beyond
/// the window that affect the CTM.
fn find_region_start(data: &[u8], pos: usize) -> (usize, bool) {
    // Simple backward scan: find the nearest line that starts with 'q' or
    // the beginning of data. We limit backward scan to 4KB for performance.
    let scan_start = pos.saturating_sub(4096);
    let region = &data[scan_start..pos];

    // Find the last unmatched q by tracking Q/q balance backwards
    let mut q_depth: i32 = 0;
    let mut best_q_pos = pos; // Default: start from text position itself
    let mut i = region.len();

    while i > 0 {
        i -= 1;
        let b = region[i];

        // Look for 'q' or 'Q' at operator boundaries
        if b == b'q' || b == b'Q' {
            let abs_pos = scan_start + i;
            // Verify it's a standalone operator (boundary check)
            let before_ok = i == 0 || {
                let prev = region[i - 1];
                prev.is_ascii_whitespace() || matches!(prev, b')' | b'>' | b']')
            };
            let after_ok = i + 1 >= region.len() || {
                let next = region[i + 1];
                next.is_ascii_whitespace()
                    || matches!(next, b'(' | b'<' | b'[' | b'/' | b'%')
                    || next.is_ascii_digit()
                    || next == b'-'
                    || next == b'.'
            };

            if before_ok && after_ok {
                if b == b'Q' {
                    q_depth += 1;
                } else {
                    // 'q'
                    if q_depth > 0 {
                        q_depth -= 1;
                    } else {
                        // Unmatched q — this is our region start
                        best_q_pos = abs_pos;
                        break;
                    }
                }
            }
        }
    }

    // We can only guarantee complete CTM context if we scanned all the way
    // to the beginning of the data. Even if we found an unmatched 'q' within
    // 4KB, there may be additional enclosing q/cm operators before the scan
    // window that establish scaling transforms we're missing.
    let hit_limit = scan_start > 0;
    (best_q_pos, hit_limit)
}

/// Scan backward from `pos` to find any immediately preceding BDC/BMC operator.
/// Returns the position of the BDC/BMC if found within 256 bytes, otherwise `pos`.
fn find_preceding_marked_content(data: &[u8], pos: usize) -> usize {
    let scan_start = pos.saturating_sub(256);
    let mut i = pos;
    while i > scan_start {
        i -= 1;
        // Look for 'C' which ends BDC or BMC
        if data[i] == b'C'
            && i >= 2
            && data[i - 2] == b'B'
            && (data[i - 1] == b'D' || data[i - 1] == b'M')
        {
            let op_start = i - 2;
            // Verify operator boundary
            let before_ok = op_start == 0 || !data[op_start - 1].is_ascii_alphanumeric();
            let after_ok = i + 1 >= data.len() || !data[i + 1].is_ascii_alphanumeric();
            if before_ok && after_ok {
                // For BDC, scan further back to include the tag and properties dict
                // e.g., "/Span << /MCID 0 >> BDC"
                // Find the start of the line/command
                let mut line_start = op_start;
                while line_start > scan_start
                    && data[line_start - 1] != b'\n'
                    && data[line_start - 1] != b'\r'
                {
                    line_start -= 1;
                }
                return line_start;
            }
        }
    }
    pos
}

/// Scan forward from `pos` to find any immediately following EMC operator.
/// Returns the position after the EMC if found within 256 bytes, otherwise `pos`.
fn find_following_emc(data: &[u8], pos: usize) -> usize {
    let scan_end = (pos + 256).min(data.len());
    let mut i = pos;
    while i + 2 < scan_end {
        if data[i] == b'E' && data[i + 1] == b'M' && data[i + 2] == b'C' {
            let before_ok = i == 0 || data[i - 1].is_ascii_whitespace();
            let after_ok = i + 3 >= data.len() || data[i + 3].is_ascii_whitespace();
            if before_ok && after_ok {
                return i + 3;
            }
        }
        i += 1;
    }
    pos
}

/// Find the position after matching "ET" for a BT starting at `start`.
fn find_matching_et(data: &[u8], start: usize) -> Option<usize> {
    let mut offset = start;
    let len = data.len();
    // Use memchr to find 'E' candidates
    loop {
        let rel = memchr::memchr(b'E', &data[offset..])?;
        let pos = offset + rel;
        offset = pos + 1;
        if pos + 1 < len && data[pos + 1] == b'T' {
            let before_ok = pos == 0
                || data[pos - 1].is_ascii_whitespace()
                || matches!(data[pos - 1], b')' | b'>' | b']' | b'}' | b'/' | b'%');
            let after_ok = pos + 2 >= len || {
                let next = data[pos + 2];
                next.is_ascii_whitespace() || matches!(next, b'(' | b'<' | b'[' | b'/' | b'%')
            };
            if before_ok && after_ok {
                return Some(pos + 2);
            }
        }
    }
}

/// Streaming text-only parser: parse operators and call handler immediately.
///
/// Same logic as `parse_content_stream_text_only` but avoids allocating a `Vec<Operator>`.
/// Each operator is passed to `handler` as soon as it's parsed, improving cache locality
/// and eliminating the intermediate operator vector (which can be 16MB+ for graphics-heavy pages).
pub fn parse_and_execute_text_only<F>(data: &[u8], mut handler: F) -> Result<()>
where
    F: FnMut(Operator) -> Result<()>,
{
    // For large streams (>256KB), use SIMD pre-scan to identify text regions.
    // This avoids byte-by-byte scanning of megabytes of path/color operators.
    if data.len() > 256 * 1024 {
        if let Some(result) = prescan_text_regions(data) {
            match result {
                PrescanResult::Empty => return Ok(()),
                PrescanResult::Regions(regions) => {
                    for (start, end) in &regions {
                        parse_region_text_only(&data[*start..*end], &mut handler)?;
                    }
                    return Ok(());
                }
                PrescanResult::RegionsWithCtm {
                    regions,
                    region_states,
                } => {
                    // Inject the correct graphics state before each BT region.
                    // Each region is wrapped in SaveState/RestoreState so state
                    // from one region doesn't leak into the next.
                    for (i, (start, end)) in regions.iter().enumerate() {
                        let state = &region_states[i];
                        let (a, b, c, d, e, f) = state.ctm;
                        handler(Operator::SaveState)?;
                        handler(Operator::Cm { a, b, c, d, e, f })?;
                        // Inject font state if the forward scan tracked one.
                        // This handles BT blocks that inherit Tf from a prior
                        // scope instead of setting their own.
                        if let Some((ref font_name, font_size)) = state.font {
                            handler(Operator::Tf {
                                font: font_name.clone(),
                                size: font_size,
                            })?;
                        }
                        parse_region_text_only(&data[*start..*end], &mut handler)?;
                        handler(Operator::RestoreState)?;
                    }
                    return Ok(());
                }
            }
        }
        // Fallback: pre-scan inconclusive, use full scan below
    }

    let mut input = data;
    let mut consecutive_errors: usize = 0;
    let mut inside_text = false;
    let mut op_count: usize = 0;

    while !input.is_empty() {
        // Skip leading whitespace (inline — both fast parser and scan_graphics
        // also handle whitespace, but this covers the initial entry and error
        // recovery paths without nom overhead).
        while !input.is_empty() && input[0].is_ascii_whitespace() {
            input = &input[1..];
        }
        if input.is_empty() {
            break;
        }

        if op_count >= effective_max_operators() {
            push_operator_cap_warning();
            break;
        }

        if inside_text {
            // Try fast path first (3-5x faster for common text operators)
            if let Some((rest, op)) = parse_text_operator_fast(input) {
                if matches!(op, Operator::EndText) {
                    inside_text = false;
                }
                handler(op)?;
                op_count += 1;
                input = rest;
                consecutive_errors = 0;
            } else {
                // Fall back to generic nom-based parser
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

    Ok(())
}

/// Parse a sub-region of a content stream for text operators.
/// Used by the pre-scan path to parse only identified text-bearing regions.
fn parse_region_text_only<F>(data: &[u8], handler: &mut F) -> Result<()>
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

/// Image-only content stream parser: skips BT/ET text blocks entirely.
///
/// Only fully parses operators relevant to image extraction:
/// `cm`, `q`, `Q`, `Do`, `BI`/`ID`/`EI` (inline images).
/// All text and graphics drawing operators are skipped.
pub fn parse_content_stream_images_only(data: &[u8]) -> Result<Vec<Operator>> {
    let operators = parse_content_stream_images_only_uncounted(data)?;
    crate::metrics::record_content_operators(operators.len());
    crate::budget::check_operator_growth(operators.len())?;
    Ok(operators)
}

fn parse_content_stream_images_only_uncounted(data: &[u8]) -> Result<Vec<Operator>> {
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
fn scan_to_et(data: &[u8]) -> Option<&[u8]> {
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

/// Parse a single operator with its operands.
///
/// Returns the remaining input and the parsed operator.
///
/// Uses `SmallVec<[Object; 6]>` for the operand buffer to avoid heap
/// allocation for the common case (most PDF operators have 0-6 operands).
/// Only spills to the heap for rare operators with more than 6 operands.
fn parse_operator_with_operands(input: &[u8]) -> IResult<&[u8], Operator> {
    // Collect operands until we hit an operator name.
    // SmallVec<[Object; 6]>: stack-allocated for <= 6 operands (covers all
    // standard PDF operators: cm/Tm need 6, most need 0-4). Only spills to
    // heap for pathological content (e.g., deeply nested arrays in Other).
    let mut operands: SmallVec<[Object; 6]> = SmallVec::new();
    let mut remaining = input;

    loop {
        // Skip whitespace
        let (inp, _) = multispace0.parse(remaining)?;
        remaining = inp;

        if remaining.is_empty() {
            return Err(nom::Err::Error(nom::error::Error::new(
                remaining,
                nom::error::ErrorKind::Eof,
            )));
        }

        // Check if this looks like an operator name (alphabetic characters)
        // Operators are typically 1-3 letter keywords
        if is_operator_start(remaining[0]) {
            let (rest, op_name) = parse_operator_name(remaining)?;

            // Special handling for inline images (BI...ID...EI sequence)
            if op_name == "BI" {
                // Parse inline image: BI <dict entries> ID <binary data> EI
                return parse_inline_image(rest);
            }

            let op = build_operator(op_name, operands);
            return Ok((rest, op));
        }

        // Otherwise, try to parse an operand (PDF object)
        let (inp, obj) = parse_object(remaining)?;
        operands.push(obj);
        remaining = inp;
    }
}

/// Check if a byte could start an operator name.
///
/// Operators start with alphabetic characters or special characters like ' or "
fn is_operator_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'\'' || byte == b'"' || byte == b'*'
}

/// Parse an operator name from the input.
///
/// Operator names are typically 1-3 letter alphabetic sequences, but can include:
/// - Single quote (') for the Quote operator
/// - Double quote (") for the DoubleQuote operator
/// - Star (*) for T* operator
fn parse_operator_name(input: &[u8]) -> IResult<&[u8], &str> {
    let (input, name_bytes) =
        take_while1(|c: u8| c.is_ascii_alphanumeric() || c == b'\'' || c == b'"' || c == b'*')
            .parse(input)?;

    let name = std::str::from_utf8(name_bytes)
        .map_err(|_| nom::Err::Error(nom::error::Error::new(input, nom::error::ErrorKind::Char)))?;

    Ok((input, name))
}

/// Build an operator from its name and operands.
///
/// This function converts the raw operator name and operands into a strongly-typed
/// Operator enum variant. It handles type conversions and validates operand counts.
///
/// Accepts `SmallVec<[Object; 6]>` to avoid heap allocation for the common case
/// (most PDF operators have 0-6 operands). The operands are consumed and dropped
/// after extraction.
fn build_operator(name: &str, operands: SmallVec<[Object; 6]>) -> Operator {
    match name {
        // Text positioning
        "Td" => {
            let tx = get_number(&operands, 0).unwrap_or(0.0);
            let ty = get_number(&operands, 1).unwrap_or(0.0);
            Operator::Td { tx, ty }
        }
        "TD" => {
            let tx = get_number(&operands, 0).unwrap_or(0.0);
            let ty = get_number(&operands, 1).unwrap_or(0.0);
            Operator::TD { tx, ty }
        }
        "Tm" => {
            let a = get_number(&operands, 0).unwrap_or(1.0);
            let b = get_number(&operands, 1).unwrap_or(0.0);
            let c = get_number(&operands, 2).unwrap_or(0.0);
            let d = get_number(&operands, 3).unwrap_or(1.0);
            let e = get_number(&operands, 4).unwrap_or(0.0);
            let f = get_number(&operands, 5).unwrap_or(0.0);
            Operator::Tm { a, b, c, d, e, f }
        }
        "T*" => Operator::TStar,

        // Text showing
        "Tj" => {
            let text = get_string(&operands, 0).unwrap_or_default();
            Operator::Tj { text }
        }
        "TJ" => {
            let elements = if let Some(array) = get_array(&operands, 0) {
                array
                    .iter()
                    .filter_map(|obj| match obj {
                        Object::String(s) => Some(TextElement::String(s.clone())),
                        Object::Integer(i) => Some(TextElement::Offset(*i as f32)),
                        Object::Real(r) => Some(TextElement::Offset(*r as f32)),
                        _ => None,
                    })
                    .collect()
            } else {
                Vec::new()
            };
            Operator::TJ { array: elements }
        }
        "'" => {
            let text = get_string(&operands, 0).unwrap_or_default();
            Operator::Quote { text }
        }
        "\"" => {
            let word_space = get_number(&operands, 0).unwrap_or(0.0);
            let char_space = get_number(&operands, 1).unwrap_or(0.0);
            let text = get_string(&operands, 2).unwrap_or_default();
            Operator::DoubleQuote {
                word_space,
                char_space,
                text,
            }
        }

        // Text state
        "Tc" => {
            let char_space = get_number(&operands, 0).unwrap_or(0.0);
            Operator::Tc { char_space }
        }
        "Tw" => {
            let word_space = get_number(&operands, 0).unwrap_or(0.0);
            Operator::Tw { word_space }
        }
        "Tz" => {
            let scale = get_number(&operands, 0).unwrap_or(100.0);
            Operator::Tz { scale }
        }
        "TL" => {
            let leading = get_number(&operands, 0).unwrap_or(0.0);
            Operator::TL { leading }
        }
        "Tf" => {
            let font = get_name(&operands, 0).unwrap_or("").to_string();
            let size = get_number(&operands, 1).unwrap_or(12.0);
            Operator::Tf { font, size }
        }
        "Tr" => {
            let render = get_integer(&operands, 0).unwrap_or(0) as u8;
            Operator::Tr { render }
        }
        "Ts" => {
            let rise = get_number(&operands, 0).unwrap_or(0.0);
            Operator::Ts { rise }
        }

        // Graphics state
        "q" => Operator::SaveState,
        "Q" => Operator::RestoreState,
        "cm" => {
            let a = get_number(&operands, 0).unwrap_or(1.0);
            let b = get_number(&operands, 1).unwrap_or(0.0);
            let c = get_number(&operands, 2).unwrap_or(0.0);
            let d = get_number(&operands, 3).unwrap_or(1.0);
            let e = get_number(&operands, 4).unwrap_or(0.0);
            let f = get_number(&operands, 5).unwrap_or(0.0);
            Operator::Cm { a, b, c, d, e, f }
        }

        // Color
        "rg" => {
            let r = get_number(&operands, 0).unwrap_or(0.0);
            let g = get_number(&operands, 1).unwrap_or(0.0);
            let b = get_number(&operands, 2).unwrap_or(0.0);
            Operator::SetFillRgb { r, g, b }
        }
        "RG" => {
            let r = get_number(&operands, 0).unwrap_or(0.0);
            let g = get_number(&operands, 1).unwrap_or(0.0);
            let b = get_number(&operands, 2).unwrap_or(0.0);
            Operator::SetStrokeRgb { r, g, b }
        }
        "g" => {
            let gray = get_number(&operands, 0).unwrap_or(0.0);
            Operator::SetFillGray { gray }
        }
        "G" => {
            let gray = get_number(&operands, 0).unwrap_or(0.0);
            Operator::SetStrokeGray { gray }
        }
        "k" => {
            // Set CMYK fill color
            let c = get_number(&operands, 0).unwrap_or(0.0);
            let m = get_number(&operands, 1).unwrap_or(0.0);
            let y = get_number(&operands, 2).unwrap_or(0.0);
            let k = get_number(&operands, 3).unwrap_or(0.0);
            Operator::SetFillCmyk { c, m, y, k }
        }
        "K" => {
            // Set CMYK stroke color
            let c = get_number(&operands, 0).unwrap_or(0.0);
            let m = get_number(&operands, 1).unwrap_or(0.0);
            let y = get_number(&operands, 2).unwrap_or(0.0);
            let k = get_number(&operands, 3).unwrap_or(0.0);
            Operator::SetStrokeCmyk { c, m, y, k }
        }

        // Color space operators
        "cs" => {
            // Set fill color space: name cs
            let name = get_name(&operands, 0).unwrap_or("DeviceGray").to_string();
            Operator::SetFillColorSpace { name }
        }
        "CS" => {
            // Set stroke color space: name CS
            let name = get_name(&operands, 0).unwrap_or("DeviceGray").to_string();
            Operator::SetStrokeColorSpace { name }
        }
        "sc" => {
            // Set fill color: c1 c2 ... cn sc
            // Number of components depends on current color space
            let components: Vec<f32> = operands
                .iter()
                .filter_map(|obj| match obj {
                    Object::Real(r) => Some(*r as f32),
                    Object::Integer(i) => Some(*i as f32),
                    _ => None,
                })
                .collect();
            Operator::SetFillColor { components }
        }
        "SC" => {
            // Set stroke color: c1 c2 ... cn SC
            let components: Vec<f32> = operands
                .iter()
                .filter_map(|obj| match obj {
                    Object::Real(r) => Some(*r as f32),
                    Object::Integer(i) => Some(*i as f32),
                    _ => None,
                })
                .collect();
            Operator::SetStrokeColor { components }
        }
        "scn" => {
            // Set fill color with pattern support: c1 c2 ... cn [name] scn
            // Last operand may be a name for pattern color spaces
            let name = if let Some(Object::Name(n)) = operands.last() {
                Some(n.clone())
            } else {
                None
            };
            let components: Vec<f32> = operands
                .iter()
                .filter_map(|obj| match obj {
                    Object::Real(r) => Some(*r as f32),
                    Object::Integer(i) => Some(*i as f32),
                    Object::Name(_) => None, // Skip pattern name
                    _ => None,
                })
                .collect();
            Operator::SetFillColorN {
                components,
                name: name.map(Box::new),
            }
        }
        "SCN" => {
            // Set stroke color with pattern support: c1 c2 ... cn [name] SCN
            let name = if let Some(Object::Name(n)) = operands.last() {
                Some(n.clone())
            } else {
                None
            };
            let components: Vec<f32> = operands
                .iter()
                .filter_map(|obj| match obj {
                    Object::Real(r) => Some(*r as f32),
                    Object::Integer(i) => Some(*i as f32),
                    Object::Name(_) => None, // Skip pattern name
                    _ => None,
                })
                .collect();
            Operator::SetStrokeColorN {
                components,
                name: name.map(Box::new),
            }
        }

        // Text object
        "BT" => Operator::BeginText,
        "ET" => Operator::EndText,

        // XObject
        "Do" => {
            // Per ISO 32000-1:2008 §7.8.2, operands "shall immediately precede"
            // their operator and none "shall be left over" once it executes.
            // Do takes exactly one operand (the XObject name); if stray operands
            // accumulated ahead of it (e.g. a dropped/malformed `cm` before this
            // `Do`), the name is still the one immediately preceding the operator,
            // i.e. the LAST element, not necessarily the first.
            let name = get_name(&operands, operands.len().saturating_sub(1))
                .unwrap_or("")
                .to_string();
            Operator::Do { name }
        }

        // Path construction
        "m" => {
            let x = get_number(&operands, 0).unwrap_or(0.0);
            let y = get_number(&operands, 1).unwrap_or(0.0);
            Operator::MoveTo { x, y }
        }
        "l" => {
            let x = get_number(&operands, 0).unwrap_or(0.0);
            let y = get_number(&operands, 1).unwrap_or(0.0);
            Operator::LineTo { x, y }
        }
        "c" => {
            // Cubic Bézier curve
            let x1 = get_number(&operands, 0).unwrap_or(0.0);
            let y1 = get_number(&operands, 1).unwrap_or(0.0);
            let x2 = get_number(&operands, 2).unwrap_or(0.0);
            let y2 = get_number(&operands, 3).unwrap_or(0.0);
            let x3 = get_number(&operands, 4).unwrap_or(0.0);
            let y3 = get_number(&operands, 5).unwrap_or(0.0);
            Operator::CurveTo {
                x1,
                y1,
                x2,
                y2,
                x3,
                y3,
            }
        }
        "v" => {
            // Bézier curve (first control point = current point)
            let x2 = get_number(&operands, 0).unwrap_or(0.0);
            let y2 = get_number(&operands, 1).unwrap_or(0.0);
            let x3 = get_number(&operands, 2).unwrap_or(0.0);
            let y3 = get_number(&operands, 3).unwrap_or(0.0);
            Operator::CurveToV { x2, y2, x3, y3 }
        }
        "y" => {
            // Bézier curve (second control point = end point)
            let x1 = get_number(&operands, 0).unwrap_or(0.0);
            let y1 = get_number(&operands, 1).unwrap_or(0.0);
            let x3 = get_number(&operands, 2).unwrap_or(0.0);
            let y3 = get_number(&operands, 3).unwrap_or(0.0);
            Operator::CurveToY { x1, y1, x3, y3 }
        }
        "h" => Operator::ClosePath,
        "re" => {
            let x = get_number(&operands, 0).unwrap_or(0.0);
            let y = get_number(&operands, 1).unwrap_or(0.0);
            let width = get_number(&operands, 2).unwrap_or(0.0);
            let height = get_number(&operands, 3).unwrap_or(0.0);
            Operator::Rectangle {
                x,
                y,
                width,
                height,
            }
        }
        "S" => Operator::Stroke,
        "f" | "F" => Operator::Fill, // "F" is obsolete equivalent of "f" (nonzero winding fill)
        "f*" => Operator::FillEvenOdd,
        "b" => Operator::CloseFillStroke,
        "b*" => Operator::CloseFillStrokeEvenOdd,
        "B" => Operator::FillStroke,
        "B*" => Operator::FillStrokeEvenOdd,
        "n" => Operator::EndPath,
        "W" => Operator::ClipNonZero,
        "W*" => Operator::ClipEvenOdd,

        // Graphics state operators
        "w" => {
            let width = get_number(&operands, 0).unwrap_or(1.0);
            Operator::SetLineWidth { width }
        }
        "d" => {
            // d operator: array phase
            // Example: [3 2] 0 d means 3 on, 2 off, starting at phase 0
            let array = if let Some(Object::Array(arr)) = operands.first() {
                arr.iter()
                    .filter_map(|obj| match obj {
                        Object::Integer(i) => Some(*i as f32),
                        Object::Real(r) => Some(*r as f32),
                        _ => None,
                    })
                    .collect()
            } else {
                Vec::new()
            };
            let phase = get_number(&operands, 1).unwrap_or(0.0);
            Operator::SetDash { array, phase }
        }
        "J" => {
            // J operator: integer J
            // 0=butt cap, 1=round cap, 2=projecting square cap
            let cap_style = get_integer(&operands, 0).unwrap_or(0) as u8;
            Operator::SetLineCap { cap_style }
        }
        "j" => {
            // j operator: integer j
            // 0=miter join, 1=round join, 2=bevel join
            let join_style = get_integer(&operands, 0).unwrap_or(0) as u8;
            Operator::SetLineJoin { join_style }
        }
        "M" => {
            // M operator: number M
            // Miter limit (ratio of miter length to line width)
            let limit = get_number(&operands, 0).unwrap_or(10.0);
            Operator::SetMiterLimit { limit }
        }
        "ri" => {
            // ri operator: name ri
            // Rendering intent: /AbsoluteColorimetric, /RelativeColorimetric, /Saturation, or /Perceptual
            let intent = get_name(&operands, 0)
                .unwrap_or("RelativeColorimetric")
                .to_string();
            Operator::SetRenderingIntent { intent }
        }
        "i" => {
            // i operator: number i
            // Flatness tolerance (0-100)
            let tolerance = get_number(&operands, 0).unwrap_or(1.0);
            Operator::SetFlatness { tolerance }
        }
        "gs" => {
            // gs operator: name gs
            // Set extended graphics state from resource dictionary
            let dict_name = get_name(&operands, 0).unwrap_or("").to_string();
            Operator::SetExtGState { dict_name }
        }
        "sh" => {
            // sh operator: name sh
            // Paint shading pattern (gradient)
            let name = get_name(&operands, 0).unwrap_or("").to_string();
            Operator::PaintShading { name }
        }

        // Marked content operators (for tagged PDF structure)
        // PDF Spec: ISO 32000-1:2008, Section 14.6
        "BMC" => {
            // Begin marked content: tag BMC
            let tag = get_name(&operands, 0).unwrap_or("").to_string();
            Operator::BeginMarkedContent { tag }
        }
        "BDC" => {
            // Begin marked content with properties: tag properties BDC
            // properties can be a dictionary or a name (reference to /Properties resource)
            let tag = get_name(&operands, 0).unwrap_or("").to_string();
            let properties = Box::new(operands.get(1).cloned().unwrap_or(Object::Null));
            Operator::BeginMarkedContentDict { tag, properties }
        }
        "EMC" => {
            // End marked content: EMC (no operands)
            Operator::EndMarkedContent
        }

        // Unknown operator — convert SmallVec to Vec for the boxed storage.
        // This path is rare (only for unrecognized operators), so the
        // conversion cost is negligible.
        _ => Operator::Other {
            name: name.to_string(),
            operands: Box::new(operands.into_vec()),
        },
    }
}

// Helper functions to extract operands

fn get_number(operands: &[Object], index: usize) -> Option<f32> {
    operands.get(index).and_then(|obj| match obj {
        Object::Integer(i) => Some(*i as f32),
        Object::Real(r) => Some(*r as f32),
        _ => None,
    })
}

fn get_integer(operands: &[Object], index: usize) -> Option<i64> {
    operands.get(index).and_then(|obj| obj.as_integer())
}

fn get_string(operands: &[Object], index: usize) -> Option<Vec<u8>> {
    operands
        .get(index)
        .and_then(|obj| obj.as_string().map(|s| s.to_vec()))
}

fn get_name(operands: &[Object], index: usize) -> Option<&str> {
    operands.get(index).and_then(|obj| obj.as_name())
}

fn get_array(operands: &[Object], index: usize) -> Option<&Vec<Object>> {
    operands.get(index).and_then(|obj| obj.as_array())
}

/// Parse an inline image sequence (BI...ID...EI).
///
/// PDF Spec: ISO 32000-1:2008, Section 8.9.7 - Inline Images
///
/// Inline images have the format:
/// BI <key value> <key value> ... ID <binary data> EI
///
/// The dictionary uses abbreviated keys:
/// - W: Width
/// - H: Height
/// - CS: ColorSpace
/// - BPC: BitsPerComponent
/// - F: Filter
/// - DP: DecodeParms
/// - I: Interpolate
///
/// The challenge is finding the EI operator in the binary data, as the bytes
/// for "EI" could appear in the image data itself. Per spec, EI must be:
/// - Preceded by whitespace (space, tab, CR, LF)
/// - Followed by whitespace or end of stream
fn parse_inline_image(input: &[u8]) -> IResult<&[u8], Operator> {
    let mut dict = HashMap::new();
    let mut remaining = input;

    // Step 1: Parse the inline image dictionary (key-value pairs)
    loop {
        // Skip whitespace
        let (inp, _) = multispace0.parse(remaining)?;
        remaining = inp;

        if remaining.is_empty() {
            return Err(nom::Err::Error(nom::error::Error::new(
                remaining,
                nom::error::ErrorKind::Eof,
            )));
        }

        // Check if we've reached "ID" (start of image data)
        if remaining.len() >= 2 && &remaining[0..2] == b"ID" {
            // Check that ID is followed by whitespace or is at end
            if remaining.len() == 2 || remaining.len() > 2 && is_whitespace(remaining[2]) {
                remaining = &remaining[2..];
                break;
            }
        }

        // Parse a key (name object, often abbreviated)
        let (inp, key_obj) = parse_object(remaining)?;
        remaining = inp;

        // Skip whitespace after key
        let (inp, _) = multispace0.parse(remaining)?;
        remaining = inp;

        // Parse the corresponding value
        let (inp, value_obj) = parse_object(remaining)?;
        remaining = inp;

        // Add to dictionary
        if let Some(key_str) = key_obj.as_name() {
            dict.insert(key_str.to_string(), value_obj);
        }
    }

    // Step 2: Skip whitespace after ID
    let (inp, _) = multispace0.parse(remaining)?;
    remaining = inp;

    // Step 3: Read binary image data until we find EI
    // EI must be preceded and followed by whitespace
    let (_inp, data) = find_and_extract_image_data(remaining)?;
    let data_len = data.len();
    remaining = &remaining[data_len..];

    // Step 4: Skip past the EI operator
    // Find EI preceded by whitespace and skip it
    let (_inp, ei_pos) = find_ei_operator(remaining)?;
    remaining = &remaining[ei_pos + 2..]; // Skip past whitespace and "EI"

    // Step 5: Return the InlineImage operator
    Ok((
        remaining,
        Operator::InlineImage {
            dict: Box::new(dict),
            data,
        },
    ))
}

/// Find the EI operator in the input, which must be preceded by whitespace.
/// Returns the position of the whitespace before EI.
fn find_ei_operator(input: &[u8]) -> IResult<&[u8], usize> {
    for i in 0..input.len().saturating_sub(2) {
        // Check if we have whitespace followed by "EI"
        if is_whitespace(input[i]) && input.len() > i + 2 && &input[i + 1..i + 3] == b"EI" {
            // Check that EI is followed by whitespace, end of stream, or another operator
            if input.len() == i + 3 || is_whitespace_or_delimiter(input[i + 3]) {
                return Ok((input, i));
            }
        }
    }

    Err(nom::Err::Error(nom::error::Error::new(
        input,
        nom::error::ErrorKind::Tag,
    )))
}

/// Extract image data up to (but not including) the whitespace before EI.
fn find_and_extract_image_data(input: &[u8]) -> IResult<&[u8], Vec<u8>> {
    let (inp, ei_pos) = find_ei_operator(input)?;
    Ok((inp, input[..ei_pos].to_vec()))
}

/// Check if a byte is whitespace (null, tab, LF, FF, CR, space — PDF spec Table 1).
fn is_whitespace(byte: u8) -> bool {
    matches!(byte, b'\x00' | b'\t' | b'\r' | b'\n' | b'\x0C' | b' ')
}

/// Check if a byte is whitespace or a PDF delimiter.
fn is_whitespace_or_delimiter(byte: u8) -> bool {
    is_whitespace(byte)
        || matches!(
            byte,
            b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
        )
}

// ── Nom-based operand skippers (test-only, superseded by raw variants) ─────

#[cfg(test)]
fn skip_operand_token(input: &[u8]) -> IResult<&[u8], ()> {
    if input.is_empty() {
        return Err(nom::Err::Error(nom::error::Error::new(
            input,
            nom::error::ErrorKind::Eof,
        )));
    }

    match input[0] {
        b'0'..=b'9' | b'.' | b'+' | b'-' => skip_number(input),
        b'(' => skip_literal_string(input),
        b'<' if input.len() > 1 && input[1] == b'<' => skip_dict(input),
        b'<' => skip_hex_string(input),
        b'/' => skip_name(input),
        b'[' => skip_array(input),
        _ => Err(nom::Err::Error(nom::error::Error::new(
            input,
            nom::error::ErrorKind::Char,
        ))),
    }
}

#[cfg(test)]
fn skip_number(input: &[u8]) -> IResult<&[u8], ()> {
    let mut i = 0;
    if i < input.len() && (input[i] == b'+' || input[i] == b'-') {
        i += 1;
    }
    let start = i;
    let mut has_dot = false;
    while i < input.len() {
        if input[i].is_ascii_digit() {
            i += 1;
        } else if input[i] == b'.' && !has_dot {
            has_dot = true;
            i += 1;
        } else {
            break;
        }
    }
    if i == start {
        return Err(nom::Err::Error(nom::error::Error::new(
            input,
            nom::error::ErrorKind::Digit,
        )));
    }
    Ok((&input[i..], ()))
}

#[cfg(test)]
fn skip_literal_string(input: &[u8]) -> IResult<&[u8], ()> {
    let mut i = 1; // past opening '('
    let mut depth: u32 = 1;
    while i < input.len() && depth > 0 {
        match input[i] {
            b'\\' if i + 1 < input.len() => i += 2,
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
    if depth != 0 {
        return Err(nom::Err::Error(nom::error::Error::new(
            input,
            nom::error::ErrorKind::Char,
        )));
    }
    Ok((&input[i..], ()))
}

#[cfg(test)]
fn skip_hex_string(input: &[u8]) -> IResult<&[u8], ()> {
    let mut i = 1; // past opening '<'
    while i < input.len() {
        if input[i] == b'>' {
            return Ok((&input[i + 1..], ()));
        }
        i += 1;
    }
    Err(nom::Err::Error(nom::error::Error::new(
        input,
        nom::error::ErrorKind::Char,
    )))
}

#[cfg(test)]
fn skip_name(input: &[u8]) -> IResult<&[u8], ()> {
    let mut i = 1; // past '/'
    while i < input.len() && !is_whitespace_or_delimiter(input[i]) {
        i += 1;
    }
    Ok((&input[i..], ()))
}

#[cfg(test)]
fn skip_array(input: &[u8]) -> IResult<&[u8], ()> {
    let mut i = 1; // past opening '['
    let mut depth: u32 = 1;
    while i < input.len() && depth > 0 {
        match input[i] {
            b'[' => {
                depth += 1;
                i += 1;
            }
            b']' => {
                depth -= 1;
                i += 1;
            }
            b'(' => {
                // Skip nested literal string
                i += 1;
                let mut str_depth: u32 = 1;
                while i < input.len() && str_depth > 0 {
                    match input[i] {
                        b'\\' if i + 1 < input.len() => i += 2,
                        b'(' => {
                            str_depth += 1;
                            i += 1;
                        }
                        b')' => {
                            str_depth -= 1;
                            i += 1;
                        }
                        _ => i += 1,
                    }
                }
            }
            b'<' if i + 1 < input.len() && input[i + 1] == b'<' => {
                // Skip nested dict <<...>>
                i += 2;
                let mut dict_depth: u32 = 1;
                while i + 1 < input.len() && dict_depth > 0 {
                    if input[i] == b'<' && input[i + 1] == b'<' {
                        dict_depth += 1;
                        i += 2;
                    } else if input[i] == b'>' && input[i + 1] == b'>' {
                        dict_depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
            }
            b'<' => {
                // Skip nested hex string
                i += 1;
                while i < input.len() && input[i] != b'>' {
                    i += 1;
                }
                if i < input.len() {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    if depth != 0 {
        return Err(nom::Err::Error(nom::error::Error::new(
            input,
            nom::error::ErrorKind::Char,
        )));
    }
    Ok((&input[i..], ()))
}

#[cfg(test)]
fn skip_dict(input: &[u8]) -> IResult<&[u8], ()> {
    let mut i = 2; // past opening '<<'
    let mut depth: u32 = 1;
    while i < input.len() && depth > 0 {
        if i + 1 < input.len() && input[i] == b'<' && input[i + 1] == b'<' {
            depth += 1;
            i += 2;
        } else if i + 1 < input.len() && input[i] == b'>' && input[i + 1] == b'>' {
            depth -= 1;
            i += 2;
        } else if input[i] == b'(' {
            // Skip literal string inside dict
            i += 1;
            let mut str_depth: u32 = 1;
            while i < input.len() && str_depth > 0 {
                match input[i] {
                    b'\\' if i + 1 < input.len() => i += 2,
                    b'(' => {
                        str_depth += 1;
                        i += 1;
                    }
                    b')' => {
                        str_depth -= 1;
                        i += 1;
                    }
                    _ => i += 1,
                }
            }
        } else if input[i] == b'<' {
            // Single '<' → hex string <...>
            i += 1;
            while i < input.len() && input[i] != b'>' {
                i += 1;
            }
            if i < input.len() {
                i += 1; // Skip closing '>'
            }
        } else {
            i += 1;
        }
    }
    if depth != 0 {
        return Err(nom::Err::Error(nom::error::Error::new(
            input,
            nom::error::ErrorKind::Char,
        )));
    }
    Ok((&input[i..], ()))
}

// ── Byte-level graphics region scanner ─────────────────────────────────────
//
// Replaces the nom-based operand loop in parse_content_stream_text_only with
// raw index arithmetic. >80% of bytes in graphics-heavy streams are digits,
// dots, and whitespace for path coordinates — a tight match loop processes
// these at near-memcpy speed vs per-operand nom IResult dispatch.

/// Result of scanning a graphics region (outside BT/ET).
enum ScanResult<'a> {
    /// All data consumed, no more operators.
    EndOfData,
    /// Found a BT operator; `rest` points past "BT".
    FoundBT { rest: &'a [u8] },
    /// Found an inline image (BI); `rest` points past "BI".
    InlineImage { rest: &'a [u8] },
    /// Found a non-skippable operator; caller should backtrack to
    /// `operand_start` for full parsing. `after_op` points past the operator
    /// name (used as fallback if full parse fails).
    NeedFullParse {
        operand_start: &'a [u8],
        after_op: &'a [u8],
    },
    /// Found a non-skippable operator (BT/BI/Do/etc.) inside a deferred q/cm
    /// block. `deferred_start` points to the first deferred `q` so the caller
    /// can full-parse the q/cm/Q sequence to preserve CTM. `trigger_start`
    /// points to the operand_start of the triggering operator so the caller
    /// resumes scanning there (the next scan_graphics_region call will
    /// immediately return the trigger via FoundBT / InlineImage / NeedFullParse).
    DeferredThenText {
        deferred_start: &'a [u8],
        trigger_start: &'a [u8],
    },
    /// A simple no-operand operator that can be emitted directly without
    /// nom parsing. Used for unmatched Q (RestoreGraphicsState) to avoid
    /// expensive full-parse fallback.
    SimpleOp { op: Operator, rest: &'a [u8] },
    /// Too many consecutive errors; remaining data is likely junk.
    TooManyErrors { remaining: &'a [u8] },
}

/// Parse N float operands from a raw byte slice.
/// Returns a fixed-size array. Returns None if not enough parseable numbers.
#[inline]
fn parse_floats<const N: usize>(data: &[u8]) -> Option<[f32; N]> {
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
fn parse_six_floats(data: &[u8]) -> Option<(f32, f32, f32, f32, f32, f32)> {
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
fn is_skippable_graphics_op_bytes(op: &[u8]) -> bool {
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
fn is_color_op_bytes(op: &[u8]) -> bool {
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

fn skip_literal_string_raw(data: &[u8], mut i: usize) -> Option<usize> {
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

fn skip_hex_string_raw(data: &[u8], mut i: usize) -> Option<usize> {
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
fn skip_name_raw(data: &[u8], mut i: usize) -> usize {
    i += 1; // past '/'
    while i < data.len() && !is_whitespace_or_delimiter(data[i]) {
        i += 1;
    }
    i
}

fn skip_array_raw(data: &[u8], i: usize) -> Option<usize> {
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

fn skip_dict_raw(data: &[u8], i: usize) -> Option<usize> {
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

/// Operand type for the fast parser's operand stack.
/// Uses `f32` for numbers and `Vec<u8>` for strings to avoid full Object creation.
enum FastOperand {
    Number(f32),
    /// Raw string bytes (already decoded from literal or hex encoding)
    StringBytes(Vec<u8>),
    /// Name string (without leading `/`)
    Name(String),
    /// Array of TextElements (for TJ operator)
    TextArray(Vec<TextElement>),
}

/// Parse a float directly from bytes. Returns (value, bytes_consumed).
#[inline]
fn parse_float_fast(data: &[u8]) -> Option<(f32, usize)> {
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
fn parse_literal_string_fast(data: &[u8], start: usize) -> Option<(Vec<u8>, usize)> {
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
fn parse_hex_string_fast(data: &[u8], start: usize) -> Option<(Vec<u8>, usize)> {
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
fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Parse a TJ array `[...]` from bytes. Returns (elements, position_after_close_bracket).
fn parse_tj_array_fast(data: &[u8], start: usize) -> Option<(Vec<TextElement>, usize)> {
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
fn parse_name_fast(data: &[u8], start: usize) -> (String, usize) {
    let mut i = start + 1; // past '/'
    let name_start = i;
    while i < data.len() && !is_whitespace_or_delimiter(data[i]) {
        i += 1;
    }
    let name = String::from_utf8_lossy(&data[name_start..i]).to_string();
    (name, i)
}

/// Fast parser for a single operator inside a BT/ET text block.
///
/// Returns `Some((remaining_input, operator))` on success, `None` on failure
/// (caller should fall back to the generic `parse_operator_with_operands`).
fn parse_text_operator_fast(input: &[u8]) -> Option<(&[u8], Operator)> {
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
const SCAN_SKIP: u8 = 0;
const SCAN_ALPHA: u8 = 1;
const SCAN_PAREN: u8 = 2;
const SCAN_ANGLE: u8 = 3;
const SCAN_BRACKET: u8 = 4;
const SCAN_SLASH: u8 = 5;
const SCAN_PERCENT: u8 = 6;
const SCAN_OTHER: u8 = 7;

static BYTE_CLASS: [u8; 256] = {
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

fn scan_graphics_region<'a>(data: &'a [u8], consecutive_errors: &mut usize) -> ScanResult<'a> {
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

#[cfg(test)]
mod tests;
