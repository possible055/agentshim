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

mod budget;
mod ctm_prescan;
mod fast_operands;
mod fast_text;
mod filtered_streams;
mod full_parser;
mod inline_images;
mod raw_scanner;
mod streaming_text;
mod text_regions;

use budget::*;
use ctm_prescan::*;
use fast_operands::*;
use fast_text::*;
use filtered_streams::*;
use full_parser::*;
use inline_images::*;
use raw_scanner::*;
use streaming_text::*;
use text_regions::*;

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

/// Graphics state snapshot captured at a text position by [`forward_scan_ctm`].
#[derive(Debug)]
struct PrescanState {
    /// Accumulated CTM components (a, b, c, d, e, f).
    ctm: (f32, f32, f32, f32, f32, f32),
    /// Current font name and size from the most recent `Tf` operator, if any.
    font: Option<(String, f32)>,
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

#[cfg(test)]
mod tests;
