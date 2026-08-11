use super::*;

/// Extract the active spot ink names + tint values from a resolved
/// `Separation` / `DeviceN` colour-space array paired with the
/// operator's component values.
///
/// Per ISO 32000-1 §8.6.6.4 / §8.6.6.5:
/// - `Separation` arrays carry one colorant name and one tint. The
///   reserved names `/All` and `/None` are surfaced verbatim so the
///   §8.6.6.3 dispatch at the call site can branch on them.
/// - `DeviceN` arrays carry an N-name colorants array. If a `/Process`
///   attributes dict declares any of those names as process channels,
///   those names are filtered out here per §8.6.6.5 — they ride the
///   CMYK plane, not a spot lane.
///
/// Returns an empty vec when:
/// - the array is malformed (no type tag, no name array),
/// - the type tag is not `Separation` or `DeviceN`,
/// - the components count does not match the colorant count.
///
/// The returned ordering matches the source declaration order so the
/// caller can pair component-index N with colorant-index N.
pub(crate) fn extract_paint_spot_inks(
    space: &Object,
    components: &[f32],
    doc: &PdfDocument,
) -> Vec<(String, f32)> {
    let arr = match space.as_array() {
        Some(a) => a,
        None => return Vec::new(),
    };
    let type_name = match arr.first().and_then(Object::as_name) {
        Some(n) => n,
        None => return Vec::new(),
    };
    let deref =
        |obj: &Object| -> Object { doc.resolve_object(obj).unwrap_or_else(|_| obj.clone()) };

    match type_name {
        "Separation" => {
            if components.is_empty() {
                return Vec::new();
            }
            let name_obj = match arr.get(1) {
                Some(o) => deref(o),
                None => return Vec::new(),
            };
            let Some(ink) = name_obj.as_name() else {
                return Vec::new();
            };
            // /All and /None are surfaced verbatim; the call site
            // branches on them per §8.6.6.3 (paint every plate at the
            // tint, or skip every plate).
            vec![(ink.to_string(), components[0])]
        }
        "Pattern" => {
            // ISO 32000-1 §8.7.3.1: a Pattern colour space may declare
            // an underlying colour space at array index 1 (uncoloured
            // tiling pattern usage). The `scn` operator carries colour
            // components for the underlying space (before the pattern
            // name); a /Separation or /DeviceN underlying space brings
            // spot-colorant identity into the paint. The spot mirror
            // needs to walk into the underlying space so a paint
            // through a Pattern with a Separation alternate writes the
            // correct spot lane.
            //
            // The `components` slice carries the underlying space's
            // tints. For uncoloured Tiling, `name` (in SetFillColorN /
            // SetStrokeColorN) provides the pattern object, but the
            // tint is the underlying space's. For Shading patterns
            // (which use the /Shading object's own /ColorSpace), the
            // `scn` typically has no components — the underlying space
            // doesn't apply to shading patterns. We rely on the
            // recursive call's behaviour: a Shading-pattern usage with
            // no underlying space (array length 1) takes the
            // `arr.get(1)` branch as None and returns empty.
            let underlying = match arr.get(1) {
                Some(o) => deref(o),
                None => return Vec::new(),
            };
            // Recurse into the underlying space. The components passed
            // through unchanged — for an uncoloured Tiling pattern,
            // they are the underlying space's source tints. For
            // patterns whose underlying is itself an array form
            // (e.g. /Pattern [/Separation /PMS185 /DeviceCMYK
            // <tint>]), the recursive call handles the /Separation
            // arm and surfaces (PMS185, components[0]).
            extract_paint_spot_inks(&underlying, components, doc)
        }
        "DeviceN" => {
            let names_obj = match arr.get(1) {
                Some(o) => deref(o),
                None => return Vec::new(),
            };
            let Some(names) = names_obj.as_array() else {
                return Vec::new();
            };
            // ISO 32000-1 §8.6.6.5 / Table 73: the optional 5th element
            // is the attributes dictionary. When its `/Process`
            // sub-dictionary declares a `/Components` array, those
            // names are PROCESS colorants. Filter them out so the spot
            // lane mirror does not write spot lanes for /Cyan,
            // /Magenta, /Yellow, /Black on a /DeviceN /Process source.
            //
            // Round 5: when /Components contains any name not present
            // in /Names, the /Process attribution is malformed per
            // §8.6.6.5 ('leading prefix' requirement). Treat /Process
            // as inert in that case — no filtering — so the spot
            // extractor returns the same result it would for a DeviceN
            // without /Process attribution. This matches the
            // `extract_process_paint_cmyk` policy (which returns None
            // and falls through). HONEST_GAP_DEVICEN_PROCESS_MISMATCHED
            // _NAMES documents the open question.
            let process_names: std::collections::HashSet<String> =
                process_names_if_valid_prefix(arr, names, &deref);

            let mut out = Vec::with_capacity(names.len());
            for (i, ink_obj) in names.iter().enumerate() {
                let Some(ink) = ink_obj.as_name() else {
                    continue;
                };
                if ink == "All" || ink == "None" {
                    continue;
                }
                if process_names.contains(ink) {
                    continue;
                }
                // Pair the colorant with its index-matched component.
                // If components vector is short the source is malformed
                // — pin tint 0 (no ink) for the missing position.
                let tint = components.get(i).copied().unwrap_or(0.0);
                out.push((ink.to_string(), tint));
            }
            out
        }
        _ => Vec::new(),
    }
}

/// Return the set of /Process /Components names ONLY when /Components
/// is a valid leading-prefix subset of /Names (§8.6.6.5). When any
/// /Components name is absent from /Names the attribution is
/// malformed; round 5 treats it as inert and returns an empty set so
/// the spot extractor surfaces every /Names entry as a spot colorant
/// — matching the no-/Process behaviour and keeping the dispatcher's
/// later RGB-inverse fallback (`extract_process_paint_cmyk` also
/// returns None on mismatched names) symmetric.
pub(super) fn process_names_if_valid_prefix(
    cs_arr: &[Object],
    names: &[Object],
    deref: &impl Fn(&Object) -> Object,
) -> std::collections::HashSet<String> {
    let proc_components = cs_arr
        .get(4)
        .map(deref)
        .as_ref()
        .and_then(Object::as_dict)
        .and_then(|attrs| attrs.get("Process"))
        .map(deref)
        .as_ref()
        .and_then(Object::as_dict)
        .and_then(|proc_dict| proc_dict.get("Components"))
        .map(deref)
        .as_ref()
        .and_then(Object::as_array)
        .map(|comps| {
            comps
                .iter()
                .filter_map(|o| o.as_name().map(str::to_string))
                .collect::<Vec<String>>()
        })
        .unwrap_or_default();
    if proc_components.is_empty() {
        return std::collections::HashSet::new();
    }
    let names_set: std::collections::HashSet<String> = names
        .iter()
        .filter_map(|o| o.as_name().map(str::to_string))
        .collect();
    if proc_components.iter().all(|c| names_set.contains(c)) {
        proc_components.into_iter().collect()
    } else {
        // Malformed /Process /Components: at least one name absent
        // from /Names. Treat /Process as inert per
        // HONEST_GAP_DEVICEN_PROCESS_MISMATCHED_NAMES.
        std::collections::HashSet::new()
    }
}

/// Process-colour reconstruction for a DeviceN paint that declares
/// `/Process` attribution (ISO 32000-1:2008 §8.6.6.5 / Table 71 + Table 72).
///
/// A DeviceN colour space may carry an `/Attributes` sub-dictionary
/// whose `/Process` entry routes a prefix of the source colorants
/// through a declared process colour space (`/DeviceCMYK`,
/// `/DeviceRGB`, `/DeviceGray`, or `/ICCBased`). For overprint /
/// transparency compositing, those process-attributed tints establish
/// the §11.7.4.3 source CMYK directly — the paint's tint transform
/// (which targets the DeviceN alternate space) is irrelevant for the
/// process attribution path because §8.6.6.5 explicitly states that
/// process components are "interpreted directly as process values by
/// consumers making use of the process dictionary".
///
/// Returns `Some((c, m, y, k))` when `space` is a `DeviceN` array with
/// a `/Process` attribute and the process colour space evaluates
/// successfully. Returns `None` for:
///  - non-`DeviceN` colour spaces (callers should handle Separation /
///    Device-family / ICC / CalGray / CalRGB explicitly),
///  - DeviceN without `/Process` attribution (the paint is a pure spot
///    paint; the process-side overprint rule is "preserve backdrop" per
///    Table 149 row 3, handled by the `SeparationOrDeviceN` class),
///  - DeviceN with a `/Process /ColorSpace` whose array form is neither
///    `/ICCBased` (N=1/3/4) nor `/Cal*` (the latter falls through to
///    the §10.3.5 RGB inverse). Real PDFs use the four device-family
///    names and `/ICCBased` overwhelmingly; the rare CalRGB/CalGray
///    cases keep the existing fallback path.
///  - DeviceN with a `/Process /Components` entry that is not present
///    in `/Names` (malformed source per §8.6.6.5; logged + None per
///    `HONEST_GAP_DEVICEN_PROCESS_MISMATCHED_NAMES`).
///
/// `/Process /ColorSpace [/ICCBased <stream>]` with N=4 takes the
/// source tints as destination CMYK directly per §8.6.6.5's "natural
/// form" wording. N=3 and N=1 follow the same shape as the named
/// `/DeviceRGB` / `/DeviceGray` arms (§10.3.5 inverse). The
/// alternate reading — round-tripping through the embedded profile's
/// CMM into sRGB and then back to destination CMYK via §10.3.5 — is
/// declined as lossy (it destroys K) and qcms 0.3.0 does not support
/// CMYK→CMYK transforms anyway. See
/// `HONEST_GAP_DEVICEN_PROCESS_ICC_PROFILE_MISMATCH` for the
/// embedded-vs-OutputIntent divergence question.
///
/// The component pairing follows §8.6.6.5: `/Process /Components`
/// entries map name-by-position to the channels of the process
/// colour space; each name's index in the parent `/Names` array picks
/// the source tint. This handles both the "all-process" case (every
/// colorant in /Names is in /Components, in canonical order) and the
/// "mixed" case (process prefix + spot tail, where the process
/// position in /Names need not be index 0 for a /DeviceN — only
/// /NChannel constrains the names to appear "sequentially").
pub(crate) fn extract_process_paint_cmyk(
    space: &Object,
    components: &[f32],
    doc: &PdfDocument,
    rendering_intent: crate::color::RenderingIntent,
    retarget_cache: Option<&crate::rendering::resolution::context::IccTransformCache>,
) -> Option<(f32, f32, f32, f32)> {
    let arr = space.as_array()?;
    if arr.first().and_then(Object::as_name)? != "DeviceN" {
        return None;
    }
    let deref =
        |obj: &Object| -> Object { doc.resolve_object(obj).unwrap_or_else(|_| obj.clone()) };

    // Parent /Names array — every colorant name appears here in source
    // declaration order. The source tints (`components`) index into
    // this array.
    let names_obj = deref(arr.get(1)?);
    let names = names_obj.as_array()?;
    let name_index = |target: &str| -> Option<usize> {
        names
            .iter()
            .enumerate()
            .find_map(|(i, o)| match o.as_name() {
                Some(n) if n == target => Some(i),
                _ => None,
            })
    };

    // /Attributes /Process sub-dictionary.
    let attrs_obj = deref(arr.get(4)?);
    let attrs = attrs_obj.as_dict()?;
    let process_obj = deref(attrs.get("Process")?);
    let process = process_obj.as_dict()?;
    let cs_obj = deref(process.get("ColorSpace")?);
    let proc_components_obj = deref(process.get("Components")?);
    let proc_components = proc_components_obj.as_array()?;

    // Pull the source tint corresponding to each /Process /Components
    // entry by looking the name up in the parent /Names array.
    //
    // §8.6.6.5 mandates that /Components names appear in /Names as a
    // leading prefix; a name absent from /Names violates the spec and
    // is unspecified reader behaviour. Round 5 fails closed (returns
    // None, the call site falls through to the §10.3.5 RGB inverse)
    // and emits a log warning so downstream tooling can flag the
    // malformed source. The matching gap constant is
    // HONEST_GAP_DEVICEN_PROCESS_MISMATCHED_NAMES in
    // `tests/test_46_round5_devicen_process_polish.rs`.
    let mut proc_tints: Vec<f32> = Vec::with_capacity(proc_components.len());
    for c in proc_components {
        let name = c.as_name()?;
        let Some(idx) = name_index(name) else {
            log::warn!(
                "DeviceN /Process /Components entry {:?} is not present in /Names; \
                 source violates ISO 32000-1 §8.6.6.5 ('leading prefix' requirement). \
                 Falling through to the §10.3.5 RGB-inverse path. See \
                 HONEST_GAP_DEVICEN_PROCESS_MISMATCHED_NAMES.",
                name
            );
            return None;
        };
        // Malformed sources with short component vectors pin missing
        // positions to 0 (no ink) — same conservative rule the spot
        // extractor uses.
        proc_tints.push(components.get(idx).copied().unwrap_or(0.0));
    }

    // Resolve the process /ColorSpace into a CMYK quadruple per
    // §10.3.5 / §8.6.4. Names may be a direct name (e.g. /DeviceCMYK)
    // or an array form (e.g. [/ICCBased <indirect-ref>]); handle the
    // four named device-family cases plus /ICCBased N=4 directly, and
    // route the rest to the caller's fallback.
    if let Some(name) = cs_obj.as_name() {
        return match name {
            "DeviceCMYK" | "CMYK" => {
                // §8.6.4.4: subtractive (c, m, y, k) — the source tints
                // ARE the source CMYK in their natural form per §8.6.6.5
                // ("values associated with the process components shall
                // be stored in their natural form").
                if proc_tints.len() < 4 {
                    return None;
                }
                Some((proc_tints[0], proc_tints[1], proc_tints[2], proc_tints[3]))
            }
            "DeviceRGB" | "RGB" => {
                // §10.3.5 additive-clamp inverse: C = 1-R, M = 1-G,
                // Y = 1-B, K = 0. Per §8.6.6.5 the process tints are
                // stored in their natural (additive) form for RGB,
                // matching §10.3.5's input convention.
                if proc_tints.len() < 3 {
                    return None;
                }
                let c = (1.0 - proc_tints[0]).clamp(0.0, 1.0);
                let m = (1.0 - proc_tints[1]).clamp(0.0, 1.0);
                let y = (1.0 - proc_tints[2]).clamp(0.0, 1.0);
                Some((c, m, y, 0.0))
            }
            "DeviceGray" | "G" => {
                // Gray → CMYK convention used by every device-space arm
                // in the renderer: K = 1 − g, C = M = Y = 0.
                if proc_tints.is_empty() {
                    return None;
                }
                let k = (1.0 - proc_tints[0]).clamp(0.0, 1.0);
                Some((0.0, 0.0, 0.0, k))
            }
            _ => None,
        };
    }

    // Array-form /Process /ColorSpace. /ICCBased is the case round 4
    // explicitly deferred (HONEST_GAP_DEVICEN_PROCESS_ICC_OVERPRINT).
    // Round 5 wires the ICCBased N=4 path: per §8.6.6.5, the process
    // tints are stored "in their natural form" — for an ICCBased CMYK
    // (N=4) process colour space the tints are subtractive CMYK in
    // the profile's CMYK space. The §11.7.4.3 dispatcher consumes
    // those tints under Table 149 row 2 ("any other process colour
    // space"). The natural-form reading preserves K and matches the
    // common production case where the embedded process profile IS
    // the document OutputIntent profile.
    //
    // The alternate reading — round-tripping through sRGB via the
    // embedded profile to recover destination CMYK via §10.3.5 —
    // destroys K and only fires when the embedded profile genuinely
    // differs from the OutputIntent. qcms 0.3.0 does not support
    // CMYK→CMYK transforms (CMYK→RGB only), so a profile-to-profile
    // retargetting is not currently available through the linked
    // CMM. HONEST_GAP_DEVICEN_PROCESS_ICC_PROFILE_MISMATCH names the
    // open question.
    //
    // N=3 (ICCBased RGB) and N=1 (ICCBased Gray) process colour
    // spaces follow the analogous device-family paths: tints in the
    // profile's source space are converted by §10.3.5 (R→C=1-R for
    // N=3; G→K=1-G for N=1). The embedded profile's tone-curve
    // adjustments are NOT applied because the round-5 reading
    // accepts tints as natural-form — exactly the spec text. This is
    // the same simplification the named /DeviceRGB and /DeviceGray
    // arms make above.
    if let Some(cs_arr) = cs_obj.as_array() {
        if cs_arr.first().and_then(Object::as_name) == Some("ICCBased") {
            let n_components = cs_arr
                .get(1)
                .map(deref)
                .as_ref()
                .and_then(Object::as_dict)
                .and_then(|d| d.get("N"))
                .and_then(Object::as_integer)
                .unwrap_or(0);
            return match n_components {
                4 => {
                    if proc_tints.len() < 4 {
                        return None;
                    }
                    // Round 7 ICC retargeting: when the active CMM
                    // backend supports CMYK→CMYK retargeting AND the
                    // embedded profile is genuinely different from the
                    // document OutputIntent profile, retarget the
                    // source tints through the destination profile's
                    // BToA. The result is the same colour the press
                    // (the OutputIntent's modelled press) would produce
                    // for the source paint, with BPC applied for the
                    // relative-colorimetric press default.
                    //
                    // Falls through to the round-5 "natural form"
                    // reading when:
                    //   - the backend can't do CMYK→CMYK (qcms 0.3),
                    //   - no OutputIntent CMYK profile is declared,
                    //   - the embedded profile parses but the
                    //     destination profile fails to parse,
                    //   - the two profiles compile to byte-identical
                    //     bytes (same press, same paint — no
                    //     conversion needed).
                    //
                    // See HONEST_GAP_DEVICEN_PROCESS_ICC_PROFILE_MISMATCH
                    // for the three-state matrix.
                    if let Some(retargeted) = try_retarget_cmyk_via_embedded_profile(
                        cs_arr,
                        &proc_tints,
                        doc,
                        rendering_intent,
                        retarget_cache,
                    ) {
                        return Some(retargeted);
                    }
                    Some((proc_tints[0], proc_tints[1], proc_tints[2], proc_tints[3]))
                }
                3 => {
                    if proc_tints.len() < 3 {
                        return None;
                    }
                    let c = (1.0 - proc_tints[0]).clamp(0.0, 1.0);
                    let m = (1.0 - proc_tints[1]).clamp(0.0, 1.0);
                    let y = (1.0 - proc_tints[2]).clamp(0.0, 1.0);
                    Some((c, m, y, 0.0))
                }
                1 => {
                    if proc_tints.is_empty() {
                        return None;
                    }
                    let k = (1.0 - proc_tints[0]).clamp(0.0, 1.0);
                    Some((0.0, 0.0, 0.0, k))
                }
                _ => None,
            };
        }
    }

    // CalRGB / CalGray / other array-form. These are uncommon
    // in DeviceN /Process attribution; routing them through the
    // proper colour transform is out of scope. The call site falls
    // back to the §10.3.5 inverse from the rasterised RGB.
    None
}

/// Closes `HONEST_GAP_DEVICEN_PROCESS_ICC_PROFILE_MISMATCH` for the
/// embedded /Process /ColorSpace [/ICCBased N=4] case.
///
/// Parses both the embedded process profile (from the /ICCBased
/// stream in `cs_arr`) and the document OutputIntent CMYK profile,
/// then runs the source tints through a `CmykRetargetTransform`
/// (which lcms2 builds as CMYK → Lab PCS → CMYK with BPC on for the
/// press default). The returned tuple is the destination-CMYK colour
/// the press would produce.
///
/// Returns `None` (so the caller falls back to the round-5 natural-
/// form reading) when:
///   - the active backend can't compile a CMYK→CMYK transform
///     (qcms 0.3 baseline — no CMYK output path),
///   - the document declares no OutputIntent CMYK profile,
///   - either profile fails to parse / cross-check the /N entry,
///   - the embedded profile's bytes match the OutputIntent profile's
///     bytes (identity retarget — no conversion needed).
///
/// The three-state HONEST_GAP_DEVICEN_PROCESS_ICC_PROFILE_MISMATCH
/// matrix in `tests/test_46_round5_devicen_process_polish.rs`
/// documents which state each (backend, profile-mismatch) tuple
/// resolves to.
fn try_retarget_cmyk_via_embedded_profile(
    cs_arr: &[Object],
    proc_tints: &[f32],
    doc: &PdfDocument,
    rendering_intent: crate::color::RenderingIntent,
    retarget_cache: Option<&crate::rendering::resolution::context::IccTransformCache>,
) -> Option<(f32, f32, f32, f32)> {
    if !crate::color::active_backend_supports_cmyk_retarget() {
        return None;
    }
    if proc_tints.len() < 4 {
        return None;
    }

    // The destination profile MUST come from the document
    // OutputIntents. Without it there's no defined target gamut to
    // retarget into, and the natural-form reading is the only
    // sensible fallback. `doc.output_intent_cmyk_profile()` already
    // performs the §14.11.5 lookup (first /GTS_PDFX or /GTS_PDFA
    // entry with a /N=4 /DestOutputProfile) and parses it through
    // IccProfile::parse, so we get a vetted Arc back.
    let dst_profile = doc.output_intent_cmyk_profile()?;

    // The embedded /Process /ColorSpace [/ICCBased N 0 R] stream
    // is at index 1 of cs_arr. Resolve the indirect reference,
    // decode the stream bytes, parse through IccProfile::parse
    // (which cross-checks N=4 against the ICC header CMYK
    // signature).
    let stream_obj = cs_arr.get(1)?;
    let resolved_stream = doc.resolve_object(stream_obj).ok()?;
    let dict = resolved_stream.as_dict()?;
    let declared_n: u8 = dict
        .get("N")
        .and_then(Object::as_integer)
        .filter(|n| *n == 4)
        .map(|n| n as u8)?;
    let bytes = resolved_stream.decode_stream_data().ok()?;
    let src_profile = std::sync::Arc::new(crate::color::IccProfile::parse(bytes, declared_n)?);

    // Identity retarget — both profiles are byte-identical, so any
    // transform we built would round-trip the input through Lab and
    // produce essentially the same bytes back (the natural-form
    // reading IS the identity retarget on byte-identical profiles).
    // Skip the transform-build cost and emit the natural form.
    if src_profile.content_hash() == dst_profile.content_hash() {
        return None;
    }

    // §10.7.3: the live `ri` operator (and any prior /RI ExtGState
    // entry) declares the rendering intent for the operator that
    // follows. The dispatcher reads `gs.rendering_intent` at paint
    // time and threads it here through `extract_process_paint_cmyk`,
    // so a `/Perceptual ri` before a /DeviceN /Process /ICCBased
    // paint retargets with the perceptual BToA tag. §8.6.5.8 pins
    // `RelativeColorimetric` as the fallback when the gs intent is
    // unset or unrecognised — that mapping is in
    // `RenderingIntent::from_pdf_name`, applied at the call site
    // before threading into here. BPC stays on for the press
    // default `TransformFlags::press_default()`.
    // Look up (or build, on miss) the compiled CMYK→CMYK retarget
    // transform through the per-renderer cache when available. Without
    // the cache, every paint re-parses both ICC profiles AND rebuilds
    // the lcms2 CLUT — for a page with thousands of process-attributed
    // DeviceN paints this is the dominant render cost. With the cache
    // the build runs once per unique (src, dst, intent) tuple and
    // every subsequent paint is a single `Arc<…>` clone. The
    // no-cache path stays around for non-rendering callers (e.g.
    // initial-colour evaluation in colour-space setup).
    let transform: Arc<crate::color::CmykRetargetTransform> = match retarget_cache {
        Some(cache) => {
            cache.get_or_build_cmyk_retarget(&src_profile, &dst_profile, rendering_intent)?
        }
        None => Arc::new(crate::color::CmykRetargetTransform::new(
            src_profile,
            dst_profile,
            rendering_intent,
        )?),
    };
    let out = transform.retarget_pixel([
        proc_tints[0].clamp(0.0, 1.0),
        proc_tints[1].clamp(0.0, 1.0),
        proc_tints[2].clamp(0.0, 1.0),
        proc_tints[3].clamp(0.0, 1.0),
    ]);
    Some((
        out[0].clamp(0.0, 1.0),
        out[1].clamp(0.0, 1.0),
        out[2].clamp(0.0, 1.0),
        out[3].clamp(0.0, 1.0),
    ))
}
