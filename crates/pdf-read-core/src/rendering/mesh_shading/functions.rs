use super::*;

// ===========================================================================
// PDF function evaluation (Types 0, 2, 3, 4) — used by `/Function`-driven
// mesh colours and by Type 1 function-based shadings.
// ===========================================================================

/// Evaluate a PDF function object against `inputs`, returning the output
/// components. Supports an array of 1-output functions plus function types
/// 0 (sampled), 2 (exponential), 3 (stitching) and 4 (PostScript). Returns
/// `None` for unsupported shapes so callers fall back to the raw inputs.
pub(super) fn eval_pdf_function(
    func: &Object,
    doc: &PdfDocument,
    inputs: &[f32],
) -> Option<Vec<f32>> {
    if let Object::Array(arr) = func {
        // An array of n single-output functions, one per colour component.
        let mut out = Vec::with_capacity(arr.len());
        for f in arr {
            let resolved = doc.resolve_object(f).ok()?;
            let mut r = eval_pdf_function(&resolved, doc, inputs)?;
            out.append(&mut r);
        }
        return Some(out);
    }

    let dict = func.as_dict()?;
    let ftype = dict.get("FunctionType").and_then(|o| o.as_integer())?;
    match ftype {
        2 => eval_type2(dict, inputs),
        3 => eval_type3(dict, doc, inputs),
        0 => eval_type0(func, dict, inputs),
        4 => eval_type4(func, dict, inputs),
        _ => None,
    }
}

/// Type 2 exponential interpolation: `f(x) = C0 + x^N (C1 - C0)`.
pub(super) fn eval_type2(dict: &HashMap<String, Object>, inputs: &[f32]) -> Option<Vec<f32>> {
    let x = *inputs.first()?;
    let c0 = dict
        .get("C0")
        .and_then(|o| o.as_array())
        .map(|a| a.iter().map(num).collect::<Vec<_>>())
        .unwrap_or_else(|| vec![0.0]);
    let c1 = dict
        .get("C1")
        .and_then(|o| o.as_array())
        .map(|a| a.iter().map(num).collect::<Vec<_>>())
        .unwrap_or_else(|| vec![1.0]);
    let n = dict.get("N").map(num).unwrap_or(1.0);
    let xp = x.abs().powf(n) * x.signum();
    let len = c0.len().max(c1.len());
    Some(
        (0..len)
            .map(|i| {
                let a = c0.get(i).copied().unwrap_or(0.0);
                let b = c1.get(i).copied().unwrap_or(0.0);
                a + xp * (b - a)
            })
            .collect(),
    )
}

/// Type 3 stitching: select the sub-function whose sub-domain contains the
/// input, remap the input through `/Encode`, and evaluate it.
fn eval_type3(
    dict: &HashMap<String, Object>,
    doc: &PdfDocument,
    inputs: &[f32],
) -> Option<Vec<f32>> {
    let x = *inputs.first()?;
    let funcs = dict.get("Functions").and_then(|o| o.as_array())?;
    if funcs.is_empty() {
        return None;
    }
    let domain = dict.get("Domain").and_then(|o| o.as_array())?;
    let (d0, d1) = (num(&domain[0]), num(domain.get(1)?));
    let bounds: Vec<f32> = dict
        .get("Bounds")
        .and_then(|o| o.as_array())
        .map(|a| a.iter().map(num).collect())
        .unwrap_or_default();
    let encode: Vec<f32> = dict
        .get("Encode")
        .and_then(|o| o.as_array())
        .map(|a| a.iter().map(num).collect())
        .unwrap_or_default();

    let xc = x.clamp(d0.min(d1), d0.max(d1));
    // Find the sub-function index `k`.
    let mut k = 0usize;
    while k < bounds.len() && xc >= bounds[k] {
        k += 1;
    }
    k = k.min(funcs.len() - 1);
    // Sub-domain [lo, hi) for segment k.
    let lo = if k == 0 { d0 } else { bounds[k - 1] };
    let hi = if k < bounds.len() { bounds[k] } else { d1 };
    let (e0, e1) = (
        encode.get(2 * k).copied().unwrap_or(0.0),
        encode.get(2 * k + 1).copied().unwrap_or(1.0),
    );
    let xe = if (hi - lo).abs() < f32::EPSILON {
        e0
    } else {
        e0 + (xc - lo) * (e1 - e0) / (hi - lo)
    };
    let sub = doc.resolve_object(&funcs[k]).ok()?;
    eval_pdf_function(&sub, doc, &[xe])
}

/// Type 4 PostScript calculator function.
fn eval_type4(func: &Object, dict: &HashMap<String, Object>, inputs: &[f32]) -> Option<Vec<f32>> {
    let bytes = func.decode_stream_data().ok()?;
    let domain = pairs(dict.get("Domain"));
    let range = pairs(dict.get("Range"));
    let in64: Vec<f64> = inputs.iter().map(|&v| v as f64).collect();
    let out = crate::functions::evaluate_type4_clamped(&bytes, &in64, &domain, &range).ok()?;
    Some(out.into_iter().map(|v| v as f32).collect())
}

/// Type 0 sampled function with multilinear interpolation. Supports up to 2
/// input dimensions (enough for Type 1 shadings and 1-D `/Function` mesh
/// colours). Bounded sample reads; returns `None` for out-of-support
/// shapes.
fn eval_type0(func: &Object, dict: &HashMap<String, Object>, inputs: &[f32]) -> Option<Vec<f32>> {
    let bytes = func.decode_stream_data().ok()?;
    let domain = pairs(dict.get("Domain"));
    let range = pairs(dict.get("Range"));
    let size: Vec<usize> = dict
        .get("Size")
        .and_then(|o| o.as_array())?
        .iter()
        .map(|o| o.as_integer().unwrap_or(0).max(0) as usize)
        .collect();
    let bps = dict.get("BitsPerSample").and_then(|o| o.as_integer())? as u32;
    let m = size.len();
    let n = range.len();
    if m == 0 || m > 2 || n == 0 || n > 8 || bps == 0 || bps > 32 {
        return None;
    }
    if size.contains(&0) || domain.len() < m {
        return None;
    }
    let encode: Vec<f32> = dict
        .get("Encode")
        .and_then(|o| o.as_array())
        .map(|a| a.iter().map(num).collect())
        .unwrap_or_else(|| {
            // Default Encode: [0 (Size_i - 1)] per input dimension.
            size.iter()
                .flat_map(|&s| [0.0, (s.saturating_sub(1)) as f32])
                .collect()
        });
    let decode: Vec<(f32, f32)> = dict
        .get("Decode")
        .and_then(|o| o.as_array())
        .map(|a| {
            a.chunks_exact(2)
                .map(|c| (num(&c[0]), num(&c[1])))
                .collect()
        })
        .unwrap_or_else(|| range.iter().map(|r| (r[0] as f32, r[1] as f32)).collect());

    // Encode each input to a continuous grid coordinate in [0, Size_i - 1].
    let mut e = [0.0f32; 2];
    for i in 0..m {
        let (d0, d1) = (domain[i][0] as f32, domain[i][1] as f32);
        let x = inputs
            .get(i)
            .copied()
            .unwrap_or(0.0)
            .clamp(d0.min(d1), d0.max(d1));
        let (en0, en1) = (
            encode.get(2 * i).copied().unwrap_or(0.0),
            encode
                .get(2 * i + 1)
                .copied()
                .unwrap_or((size[i] - 1) as f32),
        );
        let ec = if (d1 - d0).abs() < f32::EPSILON {
            en0
        } else {
            en0 + (x - d0) * (en1 - en0) / (d1 - d0)
        };
        e[i] = ec.clamp(0.0, (size[i] - 1) as f32);
    }

    let max_sample = if bps >= 32 {
        u32::MAX as f64
    } else {
        ((1u64 << bps) - 1) as f64
    };

    // Fetch one output-sample vector at integer grid coordinates.
    let sample = |coord: &[usize; 2]| -> Vec<f32> {
        let mut flat = 0usize;
        let mut stride = 1usize;
        for i in 0..m {
            flat += coord[i].min(size[i] - 1) * stride;
            stride *= size[i];
        }
        (0..n)
            .map(|o| {
                let bit_off = (flat * n + o) * bps as usize;
                let raw = read_bits_at(&bytes, bit_off, bps).unwrap_or(0);
                (raw as f64 / max_sample) as f32
            })
            .collect()
    };

    // Multilinear interpolation over the 2^m surrounding grid corners.
    let corners = 1usize << m;
    let mut acc = vec![0.0f32; n];
    for c in 0..corners {
        let mut coord = [0usize; 2];
        let mut weight = 1.0f32;
        for i in 0..m {
            let base = e[i].floor() as usize;
            let frac = e[i] - base as f32;
            let hi = (c >> i) & 1;
            if hi == 1 {
                coord[i] = (base + 1).min(size[i] - 1);
                weight *= frac;
            } else {
                coord[i] = base;
                weight *= 1.0 - frac;
            }
        }
        if weight == 0.0 {
            continue;
        }
        let s = sample(&coord);
        for o in 0..n {
            acc[o] += weight * s[o];
        }
    }

    // Map normalised samples through /Decode.
    Some(
        acc.iter()
            .enumerate()
            .map(|(o, &v)| {
                let (lo, hi) = decode.get(o).copied().unwrap_or((0.0, 1.0));
                lo + v * (hi - lo)
            })
            .collect(),
    )
}

/// Read `nbits` (≤32) MSB-first from an arbitrary bit offset. Returns
/// `None` when the range exceeds the buffer.
fn read_bits_at(bytes: &[u8], bit_off: usize, nbits: u32) -> Option<u32> {
    let nbits = nbits as usize;
    if nbits == 0 {
        return Some(0);
    }
    if bit_off + nbits > bytes.len() * 8 {
        return None;
    }
    let mut value: u32 = 0;
    for i in 0..nbits {
        let pos = bit_off + i;
        let byte = bytes[pos >> 3];
        let bit = (byte >> (7 - (pos & 7))) & 1;
        value = (value << 1) | bit as u32;
    }
    Some(value)
}

// ===========================================================================
// Small numeric helpers.
// ===========================================================================

/// Numeric coercion for `Object` (Integer or Real → f32; else 0).
pub(super) fn num(o: &Object) -> f32 {
    o.as_real()
        .map(|v| v as f32)
        .or_else(|| o.as_integer().map(|i| i as f32))
        .unwrap_or(0.0)
}

/// Flatten a `[min max min max ...]` array object into `[[min, max], ...]`.
pub(super) fn pairs(o: Option<&Object>) -> Vec<[f64; 2]> {
    o.and_then(|o| o.as_array())
        .map(|a| {
            a.chunks_exact(2)
                .map(|c| {
                    let lo = c[0]
                        .as_real()
                        .or_else(|| c[0].as_integer().map(|i| i as f64))
                        .unwrap_or(0.0);
                    let hi = c[1]
                        .as_real()
                        .or_else(|| c[1].as_integer().map(|i| i as f64))
                        .unwrap_or(0.0);
                    [lo, hi]
                })
                .collect()
        })
        .unwrap_or_default()
}
