use super::*;

/// Convert a fully-evaluated device-family colour into a final
/// [`ResolvedColor`]. Cmyk passes through as `ResolvedColor::Cmyk` so
/// per-plate backends route by channel and the OPM=1 zero-component
/// rule (§11.7.4.3) can fire on DeviceCMYK direct sources. Composite
/// consumers project Cmyk → Rgba on demand (see page_renderer's
/// `run_pipeline_for_logical`).
pub(super) fn device_to_rgba(dev: DeviceColor, alpha: f32) -> ResolvedColor {
    match dev {
        DeviceColor::Gray(g) => ResolvedColor::Rgba {
            r: g,
            g,
            b: g,
            a: alpha,
        },
        DeviceColor::Rgb(r, g, b) => ResolvedColor::Rgba { r, g, b, a: alpha },
        DeviceColor::Cmyk(c, m, y, k) => ResolvedColor::Cmyk {
            c: c.clamp(0.0, 1.0),
            m: m.clamp(0.0, 1.0),
            y: y.clamp(0.0, 1.0),
            k: k.clamp(0.0, 1.0),
            a: alpha,
        },
    }
}

pub(super) fn resolve_device_alias(name: &str, components: &[f32], alpha: f32) -> ResolvedColor {
    match name {
        "DeviceGray" | "G" | "CalGray" if !components.is_empty() => {
            first_as_gray(components, alpha)
        }
        "DeviceRGB" | "RGB" | "CalRGB" if components.len() >= 3 => three_as_rgb(components, alpha),
        "DeviceCMYK" | "CMYK" if components.len() >= 4 => four_as_cmyk_native(components, alpha),
        _ => first_as_gray(components, alpha),
    }
}

pub(super) fn first_as_gray(components: &[f32], alpha: f32) -> ResolvedColor {
    let g = components.first().copied().unwrap_or(0.0).clamp(0.0, 1.0);
    ResolvedColor::Rgba {
        r: g,
        g,
        b: g,
        a: alpha,
    }
}

pub(super) fn three_as_rgb(components: &[f32], alpha: f32) -> ResolvedColor {
    ResolvedColor::Rgba {
        r: components[0].clamp(0.0, 1.0),
        g: components[1].clamp(0.0, 1.0),
        b: components[2].clamp(0.0, 1.0),
        a: alpha,
    }
}

/// Emit `ResolvedColor::Rgba` from a 4-component CMYK via the
/// context-aware CMYK→RGB path: the document's `/OutputIntents` CMYK
/// profile when present, otherwise the process-ink conversion. Used by
/// the Separation / DeviceN alternate-CMYK projection — the per-plate
/// routing for those sources is governed by the source colour space,
/// not the alternate's CMYK decomposition, so the alt is composite-
/// only.
pub(super) fn four_as_cmyk(
    components: &[f32],
    alpha: f32,
    ctx: &ResolutionContext,
) -> ResolvedColor {
    let (r, g, b) = cmyk_to_rgb_via_intent(
        components[0],
        components[1],
        components[2],
        components[3],
        ctx,
    );
    ResolvedColor::Rgba { r, g, b, a: alpha }
}

/// Emit `ResolvedColor::Cmyk` carrying the four-channel decomposition
/// for genuine DeviceCMYK / ICCBased N=4 sources. The per-plate
/// router consumes this directly (process-ink routing + OPM=1 zero-
/// component rule); the composite path projects to RGBA via the
/// process-ink `cmyk_to_rgb_via_intent` in `run_pipeline_for_logical`.
pub(super) fn four_as_cmyk_native(components: &[f32], alpha: f32) -> ResolvedColor {
    ResolvedColor::Cmyk {
        c: components[0].clamp(0.0, 1.0),
        m: components[1].clamp(0.0, 1.0),
        y: components[2].clamp(0.0, 1.0),
        k: components[3].clamp(0.0, 1.0),
        a: alpha,
    }
}

/// DeviceCMYK → DeviceRGB via the PROCESS-INK conversion
/// (`crate::color::cmyk_to_rgb`, tetralinear over the 16 measured ink
/// corners), NOT the naive §10.3.5 additive clamp `R = 1 - min(1, C+K)`.
///
/// This is the no-OutputIntent fallback of the composite render path
/// (`run_pipeline_for_logical` → `cmyk_to_rgb_via_intent`), so it must
/// agree with the renderer's own `page_renderer::cmyk_to_rgb`, the image
/// pixel path (`extractors::images::cmyk_pixel_to_rgb`) and the
/// text/extraction path (`document.rs`/`text.rs`): the same CMYK value
/// resolves to the same RGB everywhere (100% K is `#231F20`, 100% cyan
/// `#00ADEF`). A real ICC/OutputIntent CMM still takes precedence when a
/// profile is available (see `cmyk_to_rgb_via_intent`).
pub(super) fn cmyk_to_rgb(c: f32, m: f32, y: f32, k: f32) -> (f32, f32, f32) {
    crate::color::cmyk_to_rgb(c, m, y, k)
}

/// Context-aware CMYK → RGB convergence.
///
/// Precedence inside this function (callers handle the embedded-ICC
/// case before reaching here — those paths route through
/// `ColorResolver::resolve_iccbased` instead, and the §8.6.5.6
/// `/DefaultCMYK` override fires inside `ColorResolver::resolve` before
/// any device-CMYK reaches this helper):
///
/// 1. `ctx.output_intent_cmyk` — when the document declares an
///    `/OutputIntents` array with a `/N=4` `/DestOutputProfile`,
///    convert the CMYK quadruple through that profile via the
///    `crate::color::Transform` wrapper. The active rendering intent
///    (`ctx.rendering_intent`, §10.7.3) gates which qcms intent the
///    transform is built for. The 8-bit round-trip (quantise CMYK to
///    `[u8; 4]`, run qcms, decode the resulting RGB to `f32`) is the
///    same encoding the rest of `crate::color` uses — going wider
///    here would diverge from the image-decoder path that already
///    funnels through this CMM.
///
/// 2. `ctx.output_intent_cmyk` is `None` — the document didn't
///    declare a CMYK OutputIntent (or one is present but couldn't be
///    parsed). Falls through to the process-ink `cmyk_to_rgb`
///    (`crate::color::cmyk_to_rgb`), the same conversion the renderer,
///    image and extraction paths use, so a DeviceCMYK colour resolves
///    identically whether or not a broken OutputIntent is present.
///
/// **Black-Point Compensation (BPC) and rendering-intent caveats:**
/// qcms 0.3.0 does not implement BPC and, for CMYK sources, silently
/// drops the rendering-intent parameter (see qcms `lib.rs:29-36` and
/// `transform.rs:1283-1289`). The intent value is threaded through the
/// cache key here so a future CMM upgrade that honours intent doesn't
/// silently collapse cache entries; the byte-level output, however, is
/// CURRENTLY intent-invariant for any CMYK input. The HONEST_GAP probe
/// `qa_round4_bpc_paper_white_preservation_under_relative_colorimetric`
/// in `tests/test_render_output_intent.rs` pins this — a CMM upgrade
/// will turn the probe RED at the new per-intent expected references.
///
/// Without the `icc` feature `convert_cmyk_pixel` already devolves to
/// §10.3.5 inside the CMM wrapper, so the OutputIntent path is
/// non-destructive when no real CMM is linked in. The explicit
/// `cfg(feature = "icc")` gate here is a micro-optimisation: skip
/// building the `Transform` wrapper altogether when there's no
/// chance of a real conversion.
pub(crate) fn cmyk_to_rgb_via_intent(
    c: f32,
    m: f32,
    y: f32,
    k: f32,
    ctx: &ResolutionContext<'_>,
) -> (f32, f32, f32) {
    #[cfg(feature = "icc-qcms")]
    if let Some(profile) = ctx.output_intent_cmyk {
        let c_u8 = (c.clamp(0.0, 1.0) * 255.0).round() as u8;
        let m_u8 = (m.clamp(0.0, 1.0) * 255.0).round() as u8;
        let y_u8 = (y.clamp(0.0, 1.0) * 255.0).round() as u8;
        let k_u8 = (k.clamp(0.0, 1.0) * 255.0).round() as u8;
        // The per-page IccTransformCache holds the compiled qcms
        // transform across the many `ResolutionContext` instances the
        // operator dispatcher builds inside one render. Without the
        // cache, every CMYK paint operator rebuilds the 17⁴ CLUT
        // (qcms::Transform::new_to) — that's the perf trap the cache
        // exists to eliminate. The unit-test path skips the cache
        // (`with_icc_transform_cache` is the renderer-only opt-in)
        // and pays the per-call build cost; integration tests cover
        // the cached path through render_page.
        let rgb = if let Some(cache) = ctx.icc_transform_cache {
            let transform = cache.get_or_build(profile, ctx.rendering_intent);
            transform.convert_cmyk_pixel(c_u8, m_u8, y_u8, k_u8)
        } else {
            let transform = crate::color::Transform::new_srgb_target(
                std::sync::Arc::clone(profile),
                ctx.rendering_intent,
            );
            transform.convert_cmyk_pixel(c_u8, m_u8, y_u8, k_u8)
        };
        return (
            rgb[0] as f32 / 255.0,
            rgb[1] as f32 / 255.0,
            rgb[2] as f32 / 255.0,
        );
    }
    // No OutputIntent → spec fallback. The `ctx` borrow is held through
    // the cfg-gated branch above; under the no-icc build we explicitly
    // discard it here so the compiler doesn't flag an unused parameter.
    let _ = ctx;
    cmyk_to_rgb(c, m, y, k)
}

/// Evaluate a Type 2 (exponential interpolation) function at a single input.
/// `dict` is the function dictionary (`{/FunctionType 2 /C0 [...] /C1 [...]
/// /N <exponent> /Domain [...]}`). Returns the per-output samples.
///
/// Per ISO 32000-1:2008 §7.10.3: `y_j = C0_j + x^N * (C1_j - C0_j)`.
pub(super) fn evaluate_type2(dict: &std::collections::HashMap<String, Object>, x: f32) -> Vec<f32> {
    let n = dict
        .get("N")
        .and_then(|o| o.as_real().or_else(|| o.as_integer().map(|i| i as f64)))
        .unwrap_or(1.0) as f32;
    let c0 = dict.get("C0").and_then(|o| o.as_array());
    let c1 = dict.get("C1").and_then(|o| o.as_array());

    let len = c0.map(|a| a.len()).max(c1.map(|a| a.len())).unwrap_or(1);

    let mut out = Vec::with_capacity(len);
    let x_pow = if n == 1.0 { x } else { x.powf(n) };
    for j in 0..len {
        let c0j = c0.and_then(|a| a.get(j)).map(object_to_f32).unwrap_or(0.0);
        let c1j = c1.and_then(|a| a.get(j)).map(object_to_f32).unwrap_or(1.0);
        out.push(c0j + x_pow * (c1j - c0j));
    }
    out
}

/// Evaluate a Type 4 (PostScript calculator) function via
/// [`crate::functions::Program`]. The function body is the stream content of
/// `func_obj`.
pub(super) fn evaluate_type4(func_obj: &Object, components: &[f32]) -> Result<Vec<f32>> {
    let Object::Stream { dict, .. } = func_obj else {
        // Type-4 functions must be streams per §7.10.5. If we reached this
        // arm without a stream, the function is malformed; fall back to a
        // single-component identity to keep the renderer alive.
        return Ok(components.to_vec());
    };
    let bytes = func_obj.decode_stream_data()?;
    let domain = dict
        .get("Domain")
        .and_then(|o| o.as_array())
        .map(|a| array_to_pairs(a))
        .unwrap_or_default();
    let range = dict
        .get("Range")
        .and_then(|o| o.as_array())
        .map(|a| array_to_pairs(a))
        .unwrap_or_default();
    let inputs: Vec<f64> = components.iter().map(|&v| v as f64).collect();
    let out = crate::functions::evaluate_type4_clamped(&bytes, &inputs, &domain, &range)?;
    Ok(out.into_iter().map(|v| v as f32).collect())
}

/// Evaluate a tint-transform function of Type 0, 2, 3 or 4 (SS 7.10) against
/// one or more inputs. Used for the Separation / DeviceN path; `depth` caps
/// Type 3 nesting so a self-referential /Functions array cannot recurse
/// unboundedly. Types 2 and 3 are single-input by spec (SS 7.10.3, 7.10.4)
/// and only ever consult `inputs[0]`; Type 0 honours the full input vector
/// so multi-channel DeviceN tint transforms sample all their dimensions.
/// Returns `None` for anything outside the supported envelope so the caller
/// can apply its established fallback instead of guessing.
pub(super) fn evaluate_tint_function(
    ctx: &ResolutionContext,
    func_resolved: &Object,
    inputs: &[f32],
    depth: usize,
) -> Option<Vec<f32>> {
    const MAX_TINT_DEPTH: usize = 4;
    if depth >= MAX_TINT_DEPTH {
        return None;
    }
    let dict = func_resolved.as_dict()?;
    let func_type = dict.get("FunctionType").and_then(|o| o.as_integer())?;
    let x0 = inputs.first().copied().unwrap_or(0.0);
    match func_type {
        0 => evaluate_type0_sampled(func_resolved, inputs),
        2 => Some(evaluate_type2(dict, x0)),
        3 => evaluate_type3_stitching(ctx, dict, x0, depth),
        4 => evaluate_type4(func_resolved, inputs).ok(),
        _ => None,
    }
}

/// Cap on the number of sampled-function input dimensions we'll evaluate.
/// Real-world DeviceN colorant counts top out around 8 (see the inline-cap
/// comment in `resolution/intent.rs`); `2^8 = 256` corner samples per
/// evaluation keeps multilinear interpolation cheap while still covering
/// every DeviceN tint transform seen in practice. A `/Size` array longer
/// than this is rejected rather than allocating `2^N` corners for an
/// attacker-controlled `N`.
const MAX_SAMPLED_FUNCTION_DIMS: usize = 8;

/// Evaluate a Type 0 (sampled) function (SS 7.10.2) for one or more inputs:
/// N-dimensional multilinear interpolation across the `2^N` sample-grid
/// corners nearest the input point, 8- or 16-bit samples, outputs mapped
/// through `/Range`. The common Separation / single-channel-DeviceN shape
/// is the N=1 case, which reduces to the same two-sample linear
/// interpolation this function always used. Returns `None` outside the
/// supported envelope (other bit depths, a non-default `/Encode`/`/Decode`,
/// malformed `/Domain`/`/Size`, a dimension count that doesn't match the
/// number of inputs, more dimensions than [`MAX_SAMPLED_FUNCTION_DIMS`], or
/// a truncated / oversized sample stream).
pub(super) fn evaluate_type0_sampled(func_obj: &Object, inputs: &[f32]) -> Option<Vec<f32>> {
    let Object::Stream { dict, .. } = func_obj else {
        return None;
    };
    let size = dict.get("Size").and_then(|o| o.as_array())?;
    let n_dims = size.len();
    if n_dims == 0 || n_dims != inputs.len() || n_dims > MAX_SAMPLED_FUNCTION_DIMS {
        return None;
    }
    let sizes: Vec<usize> = size.iter().map(object_to_f64).map(|v| v as usize).collect();
    if sizes.contains(&0) {
        return None;
    }
    let bps = dict
        .get("BitsPerSample")
        .and_then(|o| o.as_integer())
        .unwrap_or(8);
    if !(bps == 8 || bps == 16) {
        return None;
    }
    // Non-default /Encode or /Decode changes the sample mapping; falling back
    // beats silently evaluating with default semantics.
    if dict.contains_key("Encode") || dict.contains_key("Decode") {
        return None;
    }
    let range = dict.get("Range").and_then(|o| o.as_array())?;
    let range = array_to_pairs(range);
    let n_out = range.len();
    if n_out == 0 {
        return None;
    }
    let domain = dict
        .get("Domain")
        .and_then(|o| o.as_array())
        .map(|a| array_to_pairs(a))
        .unwrap_or_default();
    // /Domain is required by spec (one pair per input dimension); the N=1
    // case additionally tolerates an absent /Domain (defaulting to [0 1])
    // to preserve exactly the leniency this function always had for the
    // common single-input shape.
    let domain: Vec<[f64; 2]> = if domain.len() == n_dims {
        domain
    } else if n_dims == 1 && domain.is_empty() {
        vec![[0.0, 1.0]]
    } else {
        return None;
    };
    for [d0, d1] in &domain {
        if !(d0.is_finite() && d1.is_finite() && d0 <= d1) {
            return None; // f64::clamp panics on NaN bounds or min > max
        }
    }

    let raw = func_obj.decode_stream_data().ok()?;
    let bytes_per = if bps == 8 { 1usize } else { 2 };
    let total_samples = sizes
        .iter()
        .try_fold(1usize, |acc, &s| acc.checked_mul(s))?;
    let needed = total_samples.checked_mul(n_out)?.checked_mul(bytes_per)?;
    if raw.len() < needed {
        return None;
    }
    let max = if bps == 8 { 255.0 } else { 65535.0 };

    // Per-dimension: clamp the input to its domain and compute the two
    // bracketing sample indices plus the interpolation fraction between
    // them, exactly like the single-input case did per-dimension.
    struct DimPos {
        i0: usize,
        i1: usize,
        frac: f64,
    }
    let mut dims = Vec::with_capacity(n_dims);
    for d in 0..n_dims {
        let [d0, d1] = domain[d];
        let n_samples = sizes[d];
        let t = (inputs[d] as f64).clamp(d0, d1);
        let span = d1 - d0;
        let pos = if span <= f64::EPSILON {
            0.0
        } else {
            (t - d0) / span * (n_samples - 1) as f64
        };
        let i0 = (pos.floor() as usize).min(n_samples - 1);
        let i1 = (i0 + 1).min(n_samples - 1);
        let frac = pos - i0 as f64;
        dims.push(DimPos { i0, i1, frac });
    }

    // Sample layout per SS 7.10.2: the first input dimension varies fastest
    // ("Sample(0,0,...), Sample(1,0,...), Sample(2,0,...), ..."), so stride
    // 0 is 1 and each subsequent dimension's stride is the product of all
    // earlier /Size entries.
    let mut strides = vec![1usize; n_dims];
    for d in 1..n_dims {
        strides[d] = strides[d - 1] * sizes[d - 1];
    }

    let sample_at = |idx: &[usize], k: usize| -> f64 {
        let flat: usize = (0..n_dims).map(|d| idx[d] * strides[d]).sum();
        let at = (flat * n_out + k) * bytes_per;
        let v = if bps == 8 {
            raw[at] as f64
        } else {
            u16::from_be_bytes([raw[at], raw[at + 1]]) as f64
        } / max;
        let [r0, r1] = range[k];
        r0 + v * (r1 - r0)
    };

    // Multilinear interpolation: blend the 2^n_dims corners of the grid
    // cell containing the input point, weighted by the product of each
    // dimension's (1-frac)/frac depending on which side of that corner sits.
    let corner_count = 1usize << n_dims;
    let mut out = vec![0f64; n_out];
    let mut idx = vec![0usize; n_dims];
    for corner in 0..corner_count {
        let mut weight = 1.0f64;
        for d in 0..n_dims {
            if (corner >> d) & 1 == 0 {
                idx[d] = dims[d].i0;
                weight *= 1.0 - dims[d].frac;
            } else {
                idx[d] = dims[d].i1;
                weight *= dims[d].frac;
            }
        }
        if weight == 0.0 {
            continue;
        }
        for (k, acc) in out.iter_mut().enumerate() {
            *acc += weight * sample_at(&idx, k);
        }
    }
    Some(out.into_iter().map(|v| v as f32).collect())
}

/// Evaluate a Type 3 (stitching) function for ONE input (SS 7.10.4): pick the
/// sub-function whose domain slice contains `x`, remap through `/Encode`, and
/// delegate (sub-functions may be Type 0/2/4 or nested Type 3, depth-capped).
pub(super) fn evaluate_type3_stitching(
    ctx: &ResolutionContext,
    dict: &std::collections::HashMap<String, Object>,
    x: f32,
    depth: usize,
) -> Option<Vec<f32>> {
    let domain = dict.get("Domain").and_then(|o| o.as_array())?;
    let domain = array_to_pairs(domain);
    let (d0, d1) = domain.first().map(|p| (p[0], p[1]))?;
    if !(d0.is_finite() && d1.is_finite() && d0 <= d1) {
        return None;
    }
    let bounds: Vec<f64> = dict
        .get("Bounds")
        .and_then(|o| o.as_array())
        .map(|a| a.iter().map(object_to_f64).collect())
        .unwrap_or_default();
    let encode = dict
        .get("Encode")
        .and_then(|o| o.as_array())
        .map(|a| array_to_pairs(a))
        .unwrap_or_default();
    let funcs = dict.get("Functions").and_then(|o| o.as_array())?;
    if funcs.is_empty() {
        return None;
    }
    let t = (x as f64).clamp(d0, d1);
    let mut k = 0usize;
    while k < bounds.len() && t >= bounds[k] {
        k += 1;
    }
    let lo = if k == 0 { d0 } else { bounds[k - 1] };
    let hi = if k == bounds.len() { d1 } else { bounds[k] };
    let (e0, e1) = encode.get(k).map(|p| (p[0], p[1])).unwrap_or((0.0, 1.0));
    let u = if (hi - lo).abs() <= f64::EPSILON {
        e0
    } else {
        e0 + (t - lo) / (hi - lo) * (e1 - e0)
    };
    let sub = ctx.doc.resolve_object(funcs.get(k)?).ok()?;
    evaluate_tint_function(ctx, &sub, &[u as f32], depth + 1)
}

/// Flatten a `[min1 max1 min2 max2 ...]` PDF array into `[[min, max], ...]`.
pub(super) fn array_to_pairs(arr: &[Object]) -> Vec<[f64; 2]> {
    arr.chunks_exact(2)
        .map(|c| [object_to_f64(&c[0]), object_to_f64(&c[1])])
        .collect()
}

pub(super) fn object_to_f32(o: &Object) -> f32 {
    object_to_f64(o) as f32
}

pub(super) fn object_to_f64(o: &Object) -> f64 {
    o.as_real()
        .or_else(|| o.as_integer().map(|i| i as f64))
        .unwrap_or(0.0)
}
