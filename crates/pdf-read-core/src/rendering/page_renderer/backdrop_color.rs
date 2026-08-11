use super::*;

pub(super) fn obj_to_f32(o: &Object) -> Option<f32> {
    o.as_real()
        .map(|r| r as f32)
        .or_else(|| o.as_integer().map(|i| i as f32))
}

/// Evaluate a /BC backdrop colour whose component count is 5 or more,
/// against the Form XObject's /Group /CS = /DeviceN (or /NChannel).
/// Returns the RGB byte triple after the DeviceN tint transform runs
/// and the alternate-space result projects to RGB.
///
/// Per ISO 32000-1:2008 §11.6.5.2 Table 144 + §8.6.6.5 (DeviceN colour
/// spaces): the BC entry consists of `n` numbers (one per group CS
/// component), and the renderer must evaluate the group's tint
/// transform to project the BC tints into the alternate colour space
/// before any further conversion.
///
/// Returns `None` when:
///  - the Form has no /Group dict, or
///  - the Group has no /CS entry, or
///  - the CS is not a /DeviceN array, or
///  - the tint transform evaluator fails to produce a result.
pub(super) fn evaluate_devicen_bc_to_rgb(
    form_dict: &std::collections::HashMap<String, Object>,
    bc: &[f32],
    doc: &PdfDocument,
) -> Option<(u8, u8, u8)> {
    let group_obj = form_dict.get("Group")?;
    let group_resolved = doc.resolve_object(group_obj).ok()?;
    let group_dict = group_resolved.as_dict()?;
    let cs_obj = group_dict.get("CS")?;
    let cs_resolved = doc.resolve_object(cs_obj).ok()?;
    let cs_arr = match &cs_resolved {
        Object::Array(arr) => arr,
        _ => return None,
    };
    let type_name = cs_arr.first().and_then(|o| o.as_name())?;
    if type_name != "DeviceN" && type_name != "NChannel" {
        return None;
    }
    let alt_cs_obj = cs_arr.get(2)?;
    let func_obj = cs_arr.get(3)?;
    let func_resolved = doc.resolve_object(func_obj).ok()?;
    let func_dict = func_resolved.as_dict()?;

    let altspace_values: Vec<f32> = evaluate_bc_tint_function(doc, &func_resolved, func_dict, bc)?;

    // Resolve the alternate space (Name → fast path, Array → typed
    // closed-form projection per §8.6.5.2-5 / §8.6.5.5).
    let alt_resolved = doc.resolve_object(alt_cs_obj).ok()?;
    let (r, g, b) = project_bc_altspace_to_rgb(doc, &alt_resolved, &altspace_values)?;

    Some((
        (r.clamp(0.0, 1.0) * 255.0).round() as u8,
        (g.clamp(0.0, 1.0) * 255.0).round() as u8,
        (b.clamp(0.0, 1.0) * 255.0).round() as u8,
    ))
}

/// Evaluate a DeviceN tint-transform function for /BC backdrop
/// resolution, dispatching across PDF function types 0/2/3/4.
///
/// Per ISO 32000-1:2008 §7.10:
///  - **Type 0** (sampled, §7.10.2) — n-dimensional sampled function;
///    evaluated by N-linear interpolation of the surrounding 2^n
///    nearest samples in the packed CLUT stream.
///  - **Type 2** (exponential, §7.10.3) — 1→m; only `bc[0]` reaches the
///    function (Type 2 inputs are scalar by spec).
///  - **Type 3** (stitching, §7.10.4) — 1→m; only `bc[0]` reaches the
///    outer function; the per-subinterval dispatch recurses into any
///    subfunction type the parser accepts.
///  - **Type 4** (PostScript calculator, §7.10.5) — n→m via the
///    crate-private `Program` evaluator.
pub(super) fn evaluate_bc_tint_function(
    doc: &PdfDocument,
    func_resolved: &Object,
    func_dict: &std::collections::HashMap<String, Object>,
    bc: &[f32],
) -> Option<Vec<f32>> {
    let func_type = func_dict.get("FunctionType").and_then(Object::as_integer)?;
    match func_type {
        0 => evaluate_type0_multi(func_resolved, func_dict, bc),
        2 => Some(evaluate_type2_multi(
            func_dict,
            bc.first().copied().unwrap_or(0.0),
        )),
        3 => evaluate_type3_multi(doc, func_dict, bc.first().copied().unwrap_or(0.0)),
        4 => {
            let bytes = match func_resolved {
                Object::Stream { .. } => func_resolved.decode_stream_data().ok()?,
                _ => return None,
            };
            let program = crate::functions::Program::compile(&bytes).ok()?;
            let inputs: Vec<f64> = bc.iter().map(|&v| v as f64).collect();
            let result = program.evaluate(&inputs).ok()?;
            Some(result.into_iter().map(|v| v as f32).collect())
        }
        _ => None,
    }
}

/// Evaluate a Type 2 (exponential interpolation) function with scalar
/// input `x`, returning the per-output samples per §7.10.3:
/// `y_j = C0_j + x^N · (C1_j - C0_j)`.
pub(super) fn evaluate_type2_multi(
    dict: &std::collections::HashMap<String, Object>,
    x: f32,
) -> Vec<f32> {
    let n_pow = dict
        .get("N")
        .and_then(|o| o.as_real().or_else(|| o.as_integer().map(|i| i as f64)))
        .unwrap_or(1.0) as f32;
    let c0 = dict.get("C0").and_then(|o| o.as_array());
    let c1 = dict.get("C1").and_then(|o| o.as_array());
    let len = c0.map(|a| a.len()).max(c1.map(|a| a.len())).unwrap_or(1);
    let x_pow = if n_pow == 1.0 { x } else { x.powf(n_pow) };
    let mut out = Vec::with_capacity(len);
    for j in 0..len {
        let c0j = c0
            .and_then(|a| a.get(j))
            .and_then(obj_to_f32)
            .unwrap_or(0.0);
        let c1j = c1
            .and_then(|a| a.get(j))
            .and_then(obj_to_f32)
            .unwrap_or(1.0);
        out.push(c0j + x_pow * (c1j - c0j));
    }
    out
}

/// Evaluate a Type 3 (stitching) function with scalar input `x` per
/// §7.10.4. Recursively evaluates the subfunction containing `x` and
/// returns its per-output samples. Subfunctions can be any function
/// type the dispatcher accepts (Type 0/2/3/4); cyclic Type 3 ⊃ Type 3
/// chains are unusual but spec-legal and supported via the recursive
/// call back into `evaluate_bc_tint_function`.
pub(super) fn evaluate_type3_multi(
    doc: &PdfDocument,
    dict: &std::collections::HashMap<String, Object>,
    x: f32,
) -> Option<Vec<f32>> {
    let domain_arr = dict.get("Domain").and_then(|o| o.as_array())?;
    if domain_arr.len() != 2 {
        return None;
    }
    let x0 = obj_to_f32(domain_arr.first()?)?;
    let x1 = obj_to_f32(domain_arr.get(1)?)?;

    let funcs_arr = dict.get("Functions").and_then(|o| o.as_array())?;
    if funcs_arr.is_empty() {
        return None;
    }
    let k = funcs_arr.len();

    let bounds_arr = dict.get("Bounds").and_then(|o| o.as_array())?;
    if bounds_arr.len() != k - 1 {
        return None;
    }
    let mut bounds: Vec<f32> = Vec::with_capacity(k - 1);
    for b in bounds_arr {
        bounds.push(obj_to_f32(b)?);
    }

    let encode_arr = dict.get("Encode").and_then(|o| o.as_array())?;
    if encode_arr.len() != 2 * k {
        return None;
    }

    let x_clipped = x.clamp(x0, x1);
    let i = bounds
        .iter()
        .copied()
        .filter(|b| x_clipped >= *b)
        .count()
        .min(k - 1);
    let lo_i = if i == 0 { x0 } else { bounds[i - 1] };
    let hi_i = if i == k - 1 { x1 } else { bounds[i] };
    let e_lo = obj_to_f32(encode_arr.get(2 * i)?)?;
    let e_hi = obj_to_f32(encode_arr.get(2 * i + 1)?)?;
    let encoded = if (hi_i - lo_i).abs() <= f32::EPSILON {
        e_lo
    } else {
        e_lo + (x_clipped - lo_i) * (e_hi - e_lo) / (hi_i - lo_i)
    };

    let sub_obj = funcs_arr.get(i)?;
    let sub_resolved = doc.resolve_object(sub_obj).ok()?;
    let sub_dict = sub_resolved.as_dict()?;
    evaluate_bc_tint_function(doc, &sub_resolved, sub_dict, &[encoded])
}

/// Evaluate a Type 0 (sampled) function with n-dimensional input `bc`
/// per §7.10.2.
///
/// The sampled function is stored as a packed stream of m·∏Size_i
/// samples; each sample is a `BitsPerSample`-bit unsigned value laid
/// out in row-major order with input dimension 0 varying fastest. We
/// linearly remap each input via `Encode` to a continuous sample index,
/// then n-linearly interpolate among the 2^n surrounding integer-grid
/// samples and finally remap the per-output samples through `Decode`
/// into the function's output range.
///
/// Returns `None` for any shape the evaluator cannot satisfy: missing
/// /Size or /Range, /BitsPerSample outside the canonical 8-bit case
/// (other depths are spec-legal but rare for tint transforms; rejecting
/// the call lets the caller report unsupported), input arity mismatch,
/// stream too short, or any malformed array.
pub(super) fn evaluate_type0_multi(
    obj: &Object,
    dict: &std::collections::HashMap<String, Object>,
    bc: &[f32],
) -> Option<Vec<f32>> {
    let domain_arr = dict.get("Domain").and_then(|o| o.as_array())?;
    let range_arr = dict.get("Range").and_then(|o| o.as_array())?;
    if domain_arr.len() % 2 != 0 || range_arr.len() % 2 != 0 {
        return None;
    }
    let n_in = domain_arr.len() / 2;
    let n_out = range_arr.len() / 2;
    if n_in == 0 || n_out == 0 || bc.len() < n_in {
        return None;
    }

    let size_arr = dict.get("Size").and_then(|o| o.as_array())?;
    if size_arr.len() != n_in {
        return None;
    }
    let mut sizes: Vec<usize> = Vec::with_capacity(n_in);
    let mut total_samples: usize = 1;
    for s in size_arr {
        let v = s.as_integer()? as usize;
        if v == 0 {
            return None;
        }
        sizes.push(v);
        total_samples = total_samples.checked_mul(v)?;
    }
    total_samples = total_samples.checked_mul(n_out)?;

    let bps = dict
        .get("BitsPerSample")
        .and_then(Object::as_integer)
        .unwrap_or(8);
    if bps != 8 {
        // §7.10.2 admits 1/2/4/8/12/16/24/32. We accept the canonical
        // 8-bit case used by every tint-transform PDF observed in the
        // wild. Wider depths fall through to None so the caller can
        // record the unsupported case (currently the only consumer is
        // /BC, which records via parent dispatch).
        return None;
    }
    let max_sample = 255.0_f32;

    let bytes = match obj {
        Object::Stream { .. } => obj.decode_stream_data().ok()?,
        _ => return None,
    };
    if bytes.len() < total_samples {
        return None;
    }

    // Encode: linearly remap each domain input to a continuous index
    // in `[0, Size_i - 1]`. Defaults to `[0 Size_i - 1]` per spec.
    let encode_arr = dict.get("Encode").and_then(|o| o.as_array());
    let mut encoded_idx: Vec<f32> = Vec::with_capacity(n_in);
    for i in 0..n_in {
        let d_lo = obj_to_f32(domain_arr.get(2 * i)?)?;
        let d_hi = obj_to_f32(domain_arr.get(2 * i + 1)?)?;
        let (e_lo, e_hi) = if let Some(arr) = encode_arr {
            if arr.len() == 2 * n_in {
                (
                    obj_to_f32(arr.get(2 * i)?)?,
                    obj_to_f32(arr.get(2 * i + 1)?)?,
                )
            } else {
                (0.0, (sizes[i] - 1) as f32)
            }
        } else {
            (0.0, (sizes[i] - 1) as f32)
        };
        let x = bc[i].clamp(d_lo, d_hi);
        let mapped = if (d_hi - d_lo).abs() <= f32::EPSILON {
            e_lo
        } else {
            e_lo + (x - d_lo) * (e_hi - e_lo) / (d_hi - d_lo)
        };
        let clamped = mapped.clamp(0.0, (sizes[i] - 1) as f32);
        encoded_idx.push(clamped);
    }

    // N-linear interpolation among the 2^n surrounding integer-grid
    // points. `lo_i` is the floor index per dimension, `frac_i` is the
    // fractional offset toward the next grid point.
    let mut lo: Vec<usize> = Vec::with_capacity(n_in);
    let mut frac: Vec<f32> = Vec::with_capacity(n_in);
    for i in 0..n_in {
        let v = encoded_idx[i];
        let lo_i = (v.floor() as isize).max(0) as usize;
        let lo_i = lo_i.min(sizes[i] - 1);
        let f_i = if lo_i + 1 >= sizes[i] {
            0.0
        } else {
            v - lo_i as f32
        };
        lo.push(lo_i);
        frac.push(f_i);
    }

    // Stride per dimension. Dimension 0 varies fastest: stride[0] = n_out,
    // stride[i] = stride[i-1] * sizes[i-1].
    let mut strides: Vec<usize> = Vec::with_capacity(n_in);
    let mut acc = n_out;
    for size in &sizes {
        strides.push(acc);
        acc = acc.checked_mul(*size)?;
    }

    // Decode: per-output `[lo hi]` mapping the [0, 255] sample byte to
    // the function's output range. Defaults to `Range`.
    let decode_arr = dict.get("Decode").and_then(|o| o.as_array());

    let mut out = Vec::with_capacity(n_out);
    let combinations = 1usize << n_in;
    for j in 0..n_out {
        // Decode bounds for output j.
        let (d_lo, d_hi) = if let Some(arr) = decode_arr {
            if arr.len() == 2 * n_out {
                (
                    obj_to_f32(arr.get(2 * j)?)?,
                    obj_to_f32(arr.get(2 * j + 1)?)?,
                )
            } else {
                (
                    obj_to_f32(range_arr.get(2 * j)?)?,
                    obj_to_f32(range_arr.get(2 * j + 1)?)?,
                )
            }
        } else {
            (
                obj_to_f32(range_arr.get(2 * j)?)?,
                obj_to_f32(range_arr.get(2 * j + 1)?)?,
            )
        };
        let r_lo = obj_to_f32(range_arr.get(2 * j)?)?;
        let r_hi = obj_to_f32(range_arr.get(2 * j + 1)?)?;

        let mut accum = 0.0_f32;
        for c in 0..combinations {
            // For each combination of {lo, lo+1} across the n_in dims,
            // compute the offset into the packed stream and the
            // multi-linear weight (product of per-dim weights).
            let mut offset = j;
            let mut weight = 1.0_f32;
            for i in 0..n_in {
                let upper = (c >> i) & 1 == 1;
                let idx_i = if upper {
                    (lo[i] + 1).min(sizes[i] - 1)
                } else {
                    lo[i]
                };
                offset += idx_i * strides[i];
                let w_i = if upper { frac[i] } else { 1.0 - frac[i] };
                weight *= w_i;
            }
            let raw = bytes[offset] as f32;
            let decoded = d_lo + (raw / max_sample) * (d_hi - d_lo);
            accum += weight * decoded;
        }
        out.push(accum.clamp(r_lo, r_hi));
    }
    Some(out)
}

/// Project a DeviceN /BC alternate-space tuple into RGB per §8.6.5.
///
/// Supports `DeviceGray` / `DeviceRGB` / `DeviceCMYK` (Name forms and
/// short names), `[/CalGray <<dict>>]`, `[/CalRGB <<dict>>]`,
/// `[/Lab <<dict>>]`, and `[/ICCBased <stream>]` of any N. Cal* and
/// Lab use closed-form §8.6.5.2-4 projections; ICCBased delegates to
/// the linked CMM (lcms2 or qcms) — when no CMM is linked in we fall
/// back to the embedded `/Alternate` colour space recursively, per
/// §8.6.5.5.
pub(super) fn project_bc_altspace_to_rgb(
    doc: &PdfDocument,
    alt_resolved: &Object,
    values: &[f32],
) -> Option<(f32, f32, f32)> {
    // Name forms first.
    if let Some(name) = alt_resolved.as_name() {
        return match name {
            "DeviceCMYK" | "CMYK" if values.len() >= 4 => {
                Some(cmyk_to_rgb(values[0], values[1], values[2], values[3]))
            }
            "DeviceRGB" | "RGB" if values.len() >= 3 => Some((values[0], values[1], values[2])),
            "DeviceGray" | "G" if !values.is_empty() => {
                let v = values[0];
                Some((v, v, v))
            }
            _ => None,
        };
    }

    // Array forms — first element is the family name.
    let arr = match alt_resolved {
        Object::Array(a) => a,
        _ => return None,
    };
    let family = arr.first().and_then(|o| o.as_name())?;
    match family {
        "DeviceCMYK" | "CMYK" if values.len() >= 4 => {
            Some(cmyk_to_rgb(values[0], values[1], values[2], values[3]))
        }
        "DeviceRGB" | "RGB" if values.len() >= 3 => Some((values[0], values[1], values[2])),
        "DeviceGray" | "G" if !values.is_empty() => {
            let v = values[0];
            Some((v, v, v))
        }
        "CalGray" => project_cal_gray_to_rgb(arr.get(1)?, values),
        "CalRGB" => project_cal_rgb_to_rgb(arr.get(1)?, values),
        "Lab" => project_lab_to_rgb(arr.get(1)?, values),
        "ICCBased" => {
            let stream_obj = arr.get(1)?;
            let stream_resolved = doc.resolve_object(stream_obj).ok()?;
            project_iccbased_to_rgb(doc, &stream_resolved, values)
        }
        _ => None,
    }
}

/// §8.6.5.2 CalGray → linear XYZ → sRGB. The /Gamma exponent applies
/// to the input value; the result is multiplied by /WhitePoint and
/// then converted to sRGB through the standard D65 sRGB transform.
pub(super) fn project_cal_gray_to_rgb(
    dict_obj: &Object,
    values: &[f32],
) -> Option<(f32, f32, f32)> {
    let dict = dict_obj.as_dict()?;
    let g = values.first().copied().unwrap_or(0.0).clamp(0.0, 1.0);
    let gamma = dict
        .get("Gamma")
        .and_then(|o| o.as_real().or_else(|| o.as_integer().map(|i| i as f64)))
        .unwrap_or(1.0) as f32;
    let wp = read_whitepoint(dict);

    // §8.6.5.2: A_g = a^gamma; X = X_w · A_g; Y = Y_w · A_g; Z = Z_w · A_g.
    let a_g = g.powf(gamma);
    let x = wp.0 * a_g;
    let y = wp.1 * a_g;
    let z = wp.2 * a_g;
    Some(xyz_to_srgb(x, y, z))
}

/// Parse a Cal* / Lab `/WhitePoint` entry, defaulting to D65
/// (0.9505, 1.0, 1.0890) per the standard sRGB / Cal* convention when
/// the entry is missing or malformed.
pub(super) fn read_whitepoint(dict: &std::collections::HashMap<String, Object>) -> (f32, f32, f32) {
    let arr = match dict.get("WhitePoint").and_then(|o| o.as_array()) {
        Some(a) if a.len() == 3 => a,
        _ => return (0.9505, 1.0, 1.0890),
    };
    let xw = obj_to_f32(&arr[0]).unwrap_or(0.9505);
    let yw = obj_to_f32(&arr[1]).unwrap_or(1.0);
    let zw = obj_to_f32(&arr[2]).unwrap_or(1.0890);
    (xw, yw, zw)
}

/// §8.6.5.3 CalRGB → linear XYZ → sRGB. Per-channel /Gamma applied to
/// the per-channel input, then the /Matrix multiplies the gamma-applied
/// tuple into linear XYZ; XYZ → sRGB closes the chain.
pub(super) fn project_cal_rgb_to_rgb(dict_obj: &Object, values: &[f32]) -> Option<(f32, f32, f32)> {
    let dict = dict_obj.as_dict()?;
    if values.len() < 3 {
        return None;
    }
    let a = values[0].clamp(0.0, 1.0);
    let b = values[1].clamp(0.0, 1.0);
    let c = values[2].clamp(0.0, 1.0);
    let (g_r, g_g, g_b) = match dict.get("Gamma").and_then(|o| o.as_array()) {
        Some(arr) if arr.len() == 3 => (
            obj_to_f32(&arr[0]).unwrap_or(1.0),
            obj_to_f32(&arr[1]).unwrap_or(1.0),
            obj_to_f32(&arr[2]).unwrap_or(1.0),
        ),
        _ => (1.0_f32, 1.0_f32, 1.0_f32),
    };
    let mat = match dict.get("Matrix").and_then(|o| o.as_array()) {
        Some(arr) if arr.len() == 9 => {
            let mut m = [0.0_f32; 9];
            for (i, slot) in m.iter_mut().enumerate() {
                *slot = obj_to_f32(&arr[i]).unwrap_or(0.0);
            }
            m
        }
        _ => [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
    };

    // §8.6.5.3: A = a^g_a, B = b^g_b, C = c^g_c; XYZ = Matrix · (A B C)^T.
    // The matrix is stored column-major per spec (Table 64): the first
    // three entries are the X column [X_a, Y_a, Z_a], the next three
    // are the Y column, the last three are the Z column.
    let a_p = a.powf(g_r);
    let b_p = b.powf(g_g);
    let c_p = c.powf(g_b);
    let x = mat[0] * a_p + mat[3] * b_p + mat[6] * c_p;
    let y = mat[1] * a_p + mat[4] * b_p + mat[7] * c_p;
    let z = mat[2] * a_p + mat[5] * b_p + mat[8] * c_p;
    Some(xyz_to_srgb(x, y, z))
}

/// §8.6.5.4 Lab → XYZ → sRGB via the standard CIELab inverse. The
/// dictionary's /WhitePoint sets the reference white; the function
/// `f^-1(t) = t^3 if t > 6/29, else 3·(6/29)^2·(t - 4/29)`.
pub(super) fn project_lab_to_rgb(dict_obj: &Object, values: &[f32]) -> Option<(f32, f32, f32)> {
    let dict = dict_obj.as_dict()?;
    if values.len() < 3 {
        return None;
    }
    let l = values[0];
    let a = values[1];
    let b = values[2];

    let wp = read_whitepoint(dict);

    // §8.6.5.4: M = (L* + 16) / 116; L_X = M + a*/500; L_Z = M - b*/200.
    let m = (l + 16.0) / 116.0;
    let l_x = m + a / 500.0;
    let l_z = m - b / 200.0;

    pub(super) fn inv_f(t: f32) -> f32 {
        let cutoff = 6.0_f32 / 29.0;
        if t > cutoff {
            t * t * t
        } else {
            3.0 * cutoff * cutoff * (t - 4.0 / 29.0)
        }
    }

    let x = wp.0 * inv_f(l_x);
    let y = wp.1 * inv_f(m);
    let z = wp.2 * inv_f(l_z);
    Some(xyz_to_srgb(x, y, z))
}

/// Linear XYZ → sRGB via the standard ITU-R BT.709 / sRGB primaries
/// matrix and the §IEC 61966-2-1 piecewise transfer function. Inputs
/// are CIE XYZ tristimulus values normalised so Y_white = 1.
pub(super) fn xyz_to_srgb(x: f32, y: f32, z: f32) -> (f32, f32, f32) {
    // sRGB primaries matrix (D65 reference). The PDF Cal* /Lab specs
    // express XYZ tristimulus values; sRGB is the canonical output.
    let r = 3.2404542 * x - 1.5371385 * y - 0.4985314 * z;
    let g = -0.969_266 * x + 1.8760108 * y + 0.041_556 * z;
    let b = 0.0556434 * x - 0.2040259 * y + 1.0572252 * z;
    pub(super) fn gamma_compress(u: f32) -> f32 {
        let u = u.clamp(0.0, 1.0);
        if u <= 0.0031308 {
            12.92 * u
        } else {
            1.055 * u.powf(1.0 / 2.4) - 0.055
        }
    }
    (gamma_compress(r), gamma_compress(g), gamma_compress(b))
}

/// §8.6.5.5 ICCBased projection. Under a linked CMM (lcms2 or qcms),
/// build a source-profile → sRGB transform and apply it. Without a
/// linked CMM, fall back to the embedded `/Alternate` space and
/// recurse. Without a /Alternate, fall back to the device family
/// inferred from the stream's /N (DeviceGray for N=1, DeviceRGB for
/// N=3, DeviceCMYK for N=4) per §8.6.5.5.
pub(super) fn project_iccbased_to_rgb(
    doc: &PdfDocument,
    stream_resolved: &Object,
    values: &[f32],
) -> Option<(f32, f32, f32)> {
    let dict = stream_resolved.as_dict()?;
    let n = dict.get("N").and_then(|o| o.as_integer()).unwrap_or(3);

    #[cfg(feature = "icc-qcms")]
    {
        if let Ok(bytes) = stream_resolved.decode_stream_data() {
            if let Some(profile) = crate::color::IccProfile::parse(bytes, n.clamp(0, 255) as u8) {
                let profile = std::sync::Arc::new(profile);
                let intent = crate::color::RenderingIntent::default();
                let transform = crate::color::Transform::new_srgb_target(
                    std::sync::Arc::clone(&profile),
                    intent,
                );
                if transform.has_cmm() {
                    match n {
                        4 if values.len() >= 4 => {
                            let c_u8 = (values[0].clamp(0.0, 1.0) * 255.0).round() as u8;
                            let m_u8 = (values[1].clamp(0.0, 1.0) * 255.0).round() as u8;
                            let y_u8 = (values[2].clamp(0.0, 1.0) * 255.0).round() as u8;
                            let k_u8 = (values[3].clamp(0.0, 1.0) * 255.0).round() as u8;
                            let rgb = transform.convert_cmyk_pixel(c_u8, m_u8, y_u8, k_u8);
                            return Some((
                                rgb[0] as f32 / 255.0,
                                rgb[1] as f32 / 255.0,
                                rgb[2] as f32 / 255.0,
                            ));
                        }
                        3 if values.len() >= 3 => {
                            let r_u8 = (values[0].clamp(0.0, 1.0) * 255.0).round() as u8;
                            let g_u8 = (values[1].clamp(0.0, 1.0) * 255.0).round() as u8;
                            let b_u8 = (values[2].clamp(0.0, 1.0) * 255.0).round() as u8;
                            let rgb = transform.convert_rgb_buffer(&[r_u8, g_u8, b_u8]);
                            if rgb.len() >= 3 {
                                return Some((
                                    rgb[0] as f32 / 255.0,
                                    rgb[1] as f32 / 255.0,
                                    rgb[2] as f32 / 255.0,
                                ));
                            }
                        }
                        1 if !values.is_empty() => {
                            let g_u8 = (values[0].clamp(0.0, 1.0) * 255.0).round() as u8;
                            let rgb = transform.convert_gray_buffer(&[g_u8]);
                            if rgb.len() >= 3 {
                                return Some((
                                    rgb[0] as f32 / 255.0,
                                    rgb[1] as f32 / 255.0,
                                    rgb[2] as f32 / 255.0,
                                ));
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    // No CMM (or CMM declined the profile) — recurse into /Alternate.
    if let Some(alt_obj) = dict.get("Alternate") {
        let alt_resolved = doc.resolve_object(alt_obj).ok()?;
        return project_bc_altspace_to_rgb(doc, &alt_resolved, values);
    }
    // No /Alternate — synthesise the device family per /N (§8.6.5.5).
    match n {
        4 if values.len() >= 4 => Some(cmyk_to_rgb(values[0], values[1], values[2], values[3])),
        3 if values.len() >= 3 => Some((values[0], values[1], values[2])),
        1 if !values.is_empty() => Some((values[0], values[0], values[0])),
        _ => None,
    }
}
