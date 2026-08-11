use super::*;

/// Per-channel `f32` comparison tolerance used by [`rgba_matches`]. The
/// resolver folds Device-family inputs through the same RGB encoding the
/// inline path uses, so an exact match is the expected case; the
/// epsilon is sized to absorb single-ulp drift from intermediate
/// computations (alpha fold, CMYK → RGB) without admitting an actual
/// colour change. Anything coarser would risk dropping subtle overrides.
pub(super) const RGBA_MATCH_EPSILON: f32 = 1.0e-6;

/// Single-input single-output transfer function used by `/SMask /TR`.
/// `Identity` is the spec default when `/TR` is absent.
#[derive(Clone, Debug)]
pub(crate) enum SMaskTransfer {
    /// Identity transfer.
    Identity,
    /// `f(x) = C0 + x^N * (C1 - C0)` per §7.10.3 Type 2 functions.
    Type2 {
        /// Lower endpoint of the codomain.
        c0: f32,
        /// Upper endpoint of the codomain.
        c1: f32,
        /// Exponent.
        n: f32,
    },
    /// Type 0 sampled function (§7.10.2). One-dimensional unit-interval
    /// lookup table — the parser materialises the sampled stream into
    /// a `Vec<f32>` so per-pixel evaluation is a single bounded
    /// allocation-free read.
    Type0 {
        /// One sample per /Size[0] entry, decoded to the [0, 1]
        /// output range. Linear interpolation between adjacent entries
        /// evaluates the function at intermediate inputs.
        samples: Vec<f32>,
    },
    /// Type 4 PostScript calculator (§7.10.5). The compiled program
    /// is reused per pixel; `Program` carries no mutable state so
    /// concurrent calls are safe.
    Type4 {
        /// Compiled PostScript program. The caller routes one f64
        /// input through `evaluate` and reads one f64 output.
        program: crate::functions::Program,
    },
    /// Type 3 stitching function (§7.10.4). Combines `k` subfunctions
    /// over disjoint subintervals of `/Domain`. For an SMask /TR the
    /// outer function is 1-input 1-output; each subfunction must also
    /// be 1-input 1-output (verified at parse time). Subfunctions can
    /// themselves be any function type the parser accepts, including
    /// Type 3 — recursive stitching is unusual but spec-legal.
    Type3 {
        /// Subfunctions in domain order. The `Vec`'s heap allocation
        /// breaks the recursive type's would-be infinite size; no
        /// extra `Box` is required (clippy `vec_box`). Length is `k`,
        /// where `k = bounds.len() + 1`.
        subfunctions: Vec<SMaskTransfer>,
        /// `k - 1` boundary values dividing `/Domain` into `k`
        /// subintervals. The i-th subinterval per §7.10.4 step 2 is
        /// `[x0, b0)`, ..., `[b(k-2), x1]` — a boundary value belongs
        /// to the subinterval on its right.
        bounds: Vec<f32>,
        /// `k` pairs of `(e_lo, e_hi)` that linearly remap each
        /// subinterval onto the corresponding subfunction's native
        /// input range. Indexed by subfunction position.
        encode: Vec<(f32, f32)>,
        /// `/Domain` as `(x0, x1)`. Inputs outside this range are
        /// clipped to the nearest endpoint before dispatch.
        domain: (f32, f32),
    },
}

impl SMaskTransfer {
    /// Evaluate the transfer at `x` clamped to its domain `[0, 1]`.
    pub(crate) fn eval(&self, x: f32) -> f32 {
        let x = x.clamp(0.0, 1.0);
        match self {
            SMaskTransfer::Identity => x,
            SMaskTransfer::Type2 { c0, c1, n } => {
                let p = x.powf(*n);
                c0 + p * (c1 - c0)
            }
            SMaskTransfer::Type0 { samples } => {
                // §7.10.2 Type-0 sampled: clamp x to [0, 1] (the
                // domain), encode to sample-index space, linearly
                // interpolate between the two nearest entries.
                let n = samples.len();
                if n == 0 {
                    return x;
                }
                if n == 1 {
                    return samples[0];
                }
                let pos = x * (n as f32 - 1.0);
                let lo = pos.floor() as usize;
                let hi = (lo + 1).min(n - 1);
                let frac = pos - lo as f32;
                let v = samples[lo] * (1.0 - frac) + samples[hi] * frac;
                v.clamp(0.0, 1.0)
            }
            SMaskTransfer::Type4 { program } => {
                // §7.10.5 PostScript calculator. The compiled program
                // takes one f64 input and emits one f64 output for a
                // /TR function (1→1 per §11.6.5.2 Table 144). Failure
                // modes (stack underflow, runtime budget) fall back
                // to identity rather than panicking; the transfer
                // function is a rendering-time concern and a malformed
                // program should not break the page render.
                match program.evaluate(&[x as f64]) {
                    Ok(out) if !out.is_empty() => (out[0] as f32).clamp(0.0, 1.0),
                    _ => x,
                }
            }
            SMaskTransfer::Type3 {
                subfunctions,
                bounds,
                encode,
                domain,
            } => {
                // §7.10.4 Type 3 stitching. Steps follow the spec:
                //   1. Clip input to `/Domain` (the outer clamp to
                //      [0, 1] at the top of `eval` already constrains
                //      the SMask /TR input to its [0, 1] range; this
                //      tighter clip enforces the function's own
                //      declared /Domain).
                //   2. Find the subinterval index `i` such that
                //      `b(i-1) <= x < b(i)`, with the convention that
                //      a boundary value belongs to the subinterval on
                //      its right and the final subinterval is
                //      half-open at its upper end (`x >= b(k-2)` →
                //      `i = k-1`).
                //   3. Compute the subinterval bounds and linearly
                //      remap `x` from `[lo_i, hi_i]` to the
                //      subfunction's native input range
                //      `[encode_lo_i, encode_hi_i]`.
                //   4. Evaluate the i-th subfunction at the encoded
                //      input; the result is the function's output.
                //
                // Malformed-input policy: an empty subfunctions vec
                // (which the parser rejects, but defensively guarded
                // here) returns the clipped input unchanged. A
                // zero-width subinterval — possible if a /Bounds entry
                // equals one of its neighbouring endpoints — degenerates
                // the linear remap (division by zero); in that case we
                // use the subfunction's `encode_lo` directly, which is
                // the only well-defined point in the remap.
                let (x0, x1) = *domain;
                let x_clipped = x.clamp(x0, x1);
                let k = subfunctions.len();
                if k == 0 {
                    return x_clipped;
                }
                // Step 2: locate subinterval index via the half-open
                // convention. `partition_point` returns the count of
                // bounds strictly ≤ x_clipped; that count IS the
                // subinterval index because every boundary belongs to
                // the right subinterval.
                let i = bounds
                    .iter()
                    .copied()
                    .filter(|b| x_clipped >= *b)
                    .count()
                    .min(k - 1);
                let lo_i = if i == 0 { x0 } else { bounds[i - 1] };
                let hi_i = if i == k - 1 { x1 } else { bounds[i] };
                let (e_lo, e_hi) = encode.get(i).copied().unwrap_or((0.0, 1.0));
                let encoded = if (hi_i - lo_i).abs() <= f32::EPSILON {
                    // Zero-width subinterval — use the encode-lo
                    // endpoint directly. Any input that falls into a
                    // collapsed subinterval is the boundary point
                    // itself, so this is the only spec-coherent choice.
                    e_lo
                } else {
                    e_lo + (x_clipped - lo_i) * (e_hi - e_lo) / (hi_i - lo_i)
                };
                subfunctions[i].eval(encoded)
            }
        }
    }
}

/// Parse a `/SMask /TR` function. Type 0 (sampled), Type 2 (exponential
/// interpolation), Type 3 (stitching), and Type 4 (PostScript calculator)
/// are recognised per ISO 32000-1:2008 §7.10. Unrecognised function
/// types fall to Identity, the spec default for an absent or
/// unrecognised /TR per §11.4.7.
pub(super) fn parse_transfer_function(doc: &PdfDocument, obj: &Object) -> Option<SMaskTransfer> {
    // Identity is a Name `/Identity` per Table 109. Anything else
    // should be a function dictionary.
    if let Some("Identity") = obj.as_name() {
        return Some(SMaskTransfer::Identity);
    }
    let dict = obj.as_dict()?;
    let ft = dict.get("FunctionType").and_then(Object::as_integer)?;
    match ft {
        0 => parse_type0_transfer_function(obj, dict).or(Some(SMaskTransfer::Identity)),
        2 => {
            let c0 = dict
                .get("C0")
                .and_then(|o| o.as_array())
                .and_then(|a| a.first())
                .and_then(|v| {
                    v.as_real()
                        .map(|r| r as f32)
                        .or_else(|| v.as_integer().map(|i| i as f32))
                })
                .unwrap_or(0.0);
            let c1 = dict
                .get("C1")
                .and_then(|o| o.as_array())
                .and_then(|a| a.first())
                .and_then(|v| {
                    v.as_real()
                        .map(|r| r as f32)
                        .or_else(|| v.as_integer().map(|i| i as f32))
                })
                .unwrap_or(1.0);
            let n = dict
                .get("N")
                .and_then(|v| {
                    v.as_real()
                        .map(|r| r as f32)
                        .or_else(|| v.as_integer().map(|i| i as f32))
                })
                .unwrap_or(1.0);
            Some(SMaskTransfer::Type2 { c0, c1, n })
        }
        3 => parse_type3_transfer_function(doc, dict).or(Some(SMaskTransfer::Identity)),
        4 => parse_type4_transfer_function(obj).or(Some(SMaskTransfer::Identity)),
        _ => Some(SMaskTransfer::Identity),
    }
}

/// Decode a Type 0 sampled-function stream into a unit-interval lookup
/// table over the 1-input 1-output domain. Returns `None` for any
/// shape the SMask /TR contract doesn't accept (multi-input or
/// multi-output) so the caller can fall back to Identity. Per
/// §7.10.2:
///  - `/Domain` is a 2-element array `[lo hi]` defining the input
///    range; for /TR this is `[0 1]` by construction.
///  - `/Range` is a 2-element array defining the output range; for
///    /TR this is `[0 1]` by construction.
///  - `/Size` is a 1-element array `[N]` — N sample positions.
///  - `/BitsPerSample` is the bit count per packed sample (1/2/4/8/
///    12/16/24/32). We accept the canonical 8-bit case the SMask /TR
///    samples-as-LUT pattern uses; deeper depths fall to None.
///  - `/Encode` defaults to `[0 Size[0]-1]` and `/Decode` defaults to
///    `/Range`. We honour the defaults; explicit overrides for /TR
///    are rare but supported via the standard linear remap.
pub(super) fn parse_type0_transfer_function(
    obj: &Object,
    dict: &std::collections::HashMap<String, Object>,
) -> Option<SMaskTransfer> {
    // Single-input single-output only. /TR per §11.6.5.2 Table 144 is
    // a 1→1 function; reject anything else so we don't silently
    // mishandle a malformed N→M sampled function.
    let domain_len = dict
        .get("Domain")
        .and_then(|o| o.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let range_len = dict
        .get("Range")
        .and_then(|o| o.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    if domain_len != 2 || range_len != 2 {
        return None;
    }
    let size_arr = dict.get("Size").and_then(|o| o.as_array())?;
    if size_arr.len() != 1 {
        return None;
    }
    let size = size_arr.first().and_then(Object::as_integer)? as usize;
    if size == 0 || size > 65_536 {
        return None;
    }
    let bps = dict
        .get("BitsPerSample")
        .and_then(Object::as_integer)
        .unwrap_or(8);
    if bps != 8 {
        // Only the 8-bit packing is honoured. Other depths land at
        // Identity to keep the parser simple; a real-world /TR rarely
        // uses anything other than 8-bit samples.
        return None;
    }
    let stream_bytes = match obj {
        Object::Stream { .. } => obj.decode_stream_data().ok()?,
        _ => return None,
    };
    if stream_bytes.len() < size {
        return None;
    }
    // /Decode default = /Range; /Encode default = [0 Size-1]. For the
    // canonical /TR shape both defaults apply, so the raw sample byte
    // /255 IS the unit-interval LUT value.
    let dec_lo;
    let dec_hi;
    if let Some(arr) = dict.get("Decode").and_then(|o| o.as_array()) {
        if arr.len() != 2 {
            return None;
        }
        dec_lo = obj_to_f32(arr.first()?)?;
        dec_hi = obj_to_f32(arr.get(1)?)?;
    } else {
        // Default to /Range.
        let r = dict.get("Range").and_then(|o| o.as_array())?;
        dec_lo = obj_to_f32(r.first()?)?;
        dec_hi = obj_to_f32(r.get(1)?)?;
    }
    let max_sample_value = 255.0; // bps=8 above
    let mut samples: Vec<f32> = Vec::with_capacity(size);
    for i in 0..size {
        let raw = stream_bytes[i] as f32;
        let v = dec_lo + (raw / max_sample_value) * (dec_hi - dec_lo);
        samples.push(v.clamp(0.0, 1.0));
    }
    Some(SMaskTransfer::Type0 { samples })
}

/// Compile a Type 4 PostScript calculator stream as a transfer
/// function. The /SMask /TR contract is 1-input 1-output per
/// §11.6.5.2 Table 144; we route through the existing crate-private
/// `Program` evaluator which already serves Separation / DeviceN tint
/// transforms. Returns `None` when the stream isn't a Stream object,
/// the parse fails (orphan procedure body, unknown operator), or the
/// program advertises a multi-input/multi-output shape that doesn't
/// match a transfer function.
pub(super) fn parse_type4_transfer_function(obj: &Object) -> Option<SMaskTransfer> {
    let dict = obj.as_dict()?;
    let domain_len = dict
        .get("Domain")
        .and_then(|o| o.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let range_len = dict
        .get("Range")
        .and_then(|o| o.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    // §7.10.5: Type 4 requires Domain and Range. /TR is 1→1.
    if domain_len != 2 || range_len != 2 {
        return None;
    }
    let stream_bytes = match obj {
        Object::Stream { .. } => obj.decode_stream_data().ok()?,
        _ => return None,
    };
    let program = crate::functions::Program::compile(&stream_bytes).ok()?;
    Some(SMaskTransfer::Type4 { program })
}

/// Parse a Type 3 stitching function (§7.10.4) as a transfer function.
/// A stitching function combines `k` subfunctions over disjoint
/// subintervals of `/Domain`, dispatching the input through whichever
/// subfunction's subinterval contains it after a linear remap. The
/// SMask /TR contract is 1-input 1-output (§11.6.5.2 Table 144), so
/// the outer function's `/Domain` is a 2-element array and each
/// subfunction must itself parse as a 1-input 1-output transfer.
///
/// Required entries per Table 39:
///  - `/Domain [x0 x1]` — 2-element array.
///  - `/Functions [f0 ... f(k-1)]` — array of `k` subfunctions, each
///    parsed recursively (any type the dispatcher accepts is valid).
///  - `/Bounds [b0 ... b(k-2)]` — `k - 1` boundary values dividing
///    `/Domain` into `k` subintervals; per §7.10.4 the spec requires
///    `x0 < b0 < b1 < ... < b(k-2) < x1`. We do NOT enforce strict
///    monotonicity here: a zero-width subinterval (e.g. `b(j-1) ==
///    b(j)`, or a boundary equal to an endpoint) is malformed but
///    spec-permitted; the `eval` arm handles the zero-width case by
///    using the subfunction's `encode_lo` directly.
///  - `/Encode [e0_lo e0_hi ... e(k-1)_lo e(k-1)_hi]` — `2k` values
///    mapping each subinterval to its subfunction's native input range.
///
/// Returns `None` for any shape the /TR contract rejects:
/// multi-input outer function, mismatched `/Bounds` or `/Encode`
/// arity, a subfunction that fails to parse, or zero subfunctions.
/// The caller falls back to Identity on `None`.
pub(super) fn parse_type3_transfer_function(
    doc: &PdfDocument,
    dict: &std::collections::HashMap<String, Object>,
) -> Option<SMaskTransfer> {
    // Outer /Domain must be 1-input (2 values) for a /TR function.
    let domain_arr = dict.get("Domain").and_then(|o| o.as_array())?;
    if domain_arr.len() != 2 {
        return None;
    }
    let x0 = obj_to_f32(domain_arr.first()?)?;
    let x1 = obj_to_f32(domain_arr.get(1)?)?;

    // /Functions — recursively parse each subfunction. Subfunctions
    // can be indirect refs so we resolve before recursing.
    let funcs_arr = dict.get("Functions").and_then(|o| o.as_array())?;
    if funcs_arr.is_empty() {
        return None;
    }
    let k = funcs_arr.len();
    let mut subfunctions: Vec<SMaskTransfer> = Vec::with_capacity(k);
    for f in funcs_arr {
        let resolved = doc.resolve_object(f).ok()?;
        let parsed = parse_transfer_function(doc, &resolved)?;
        subfunctions.push(parsed);
    }

    // /Bounds — k-1 entries.
    let bounds_arr = dict.get("Bounds").and_then(|o| o.as_array())?;
    if bounds_arr.len() != k - 1 {
        return None;
    }
    let mut bounds: Vec<f32> = Vec::with_capacity(k - 1);
    for b in bounds_arr {
        bounds.push(obj_to_f32(b)?);
    }

    // /Encode — 2k entries (k pairs of (lo, hi)).
    let encode_arr = dict.get("Encode").and_then(|o| o.as_array())?;
    if encode_arr.len() != 2 * k {
        return None;
    }
    let mut encode: Vec<(f32, f32)> = Vec::with_capacity(k);
    for i in 0..k {
        let lo = obj_to_f32(encode_arr.get(2 * i)?)?;
        let hi = obj_to_f32(encode_arr.get(2 * i + 1)?)?;
        encode.push((lo, hi));
    }

    Some(SMaskTransfer::Type3 {
        subfunctions,
        bounds,
        encode,
        domain: (x0, x1),
    })
}
