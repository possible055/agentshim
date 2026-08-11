use super::*;

/// Discover the set of `/Separation` and `/DeviceN` spot colorants
/// declared on `page_index` and within any nested Form XObject
/// `/Resources/ColorSpace` reached through `Do` operators in the
/// page's content stream.
///
/// Round 1 wraps [`PdfDocument::get_page_inks_deep`] so the sidecar's
/// spot set matches the spot set the separation renderer's per-plate
/// path already allocates. The walker filters `/All` and `/None` per
/// §8.6.6.4, sorts ASCII, and dedups. The result is stable across
/// renders of the same page.
///
/// Returns an empty vector when the page declares no spot colorants
/// (including the common case of a CMYK-only press job whose only
/// inks are the four process colorants Cyan / Magenta / Yellow /
/// Black). The four process inks are NOT surfaced here — they live
/// on the CMYK plane, not in the spot list.
///
/// # Error handling
///
/// On a parse error, malformed colorant array, or recursion-bound
/// trip from [`PdfDocument::get_page_inks_deep`], this function emits
/// a `log::warn!` naming the page and the underlying error, then
/// returns an empty vector. The render continues with degraded spot
/// fidelity (the sidecar allocates a zero-length spot stack and any
/// downstream paint-op writes that target spot lanes will find no
/// lane to write to — i.e. the spot ink quietly drops out of the
/// composite). This matches how the separation renderer handles the
/// same error (its per-plate path also degrades on a malformed
/// resource tree). The warning is the diagnostic signal that lets the
/// caller see the silent fidelity loss in a log scrape.
pub(crate) fn discover_page_spot_inks(doc: &PdfDocument, page_index: usize) -> Vec<String> {
    // get_page_inks_deep already enforces the §8.6.6.4 rules: filters
    // /All and /None, dedups, sorts. On error, surface via log::warn
    // so the silent-degradation is visible to the host application's
    // log pipeline — a silent unwrap_or_default would let the spot
    // lanes drop out of the composite without any signal.
    match doc.get_page_inks_deep(page_index) {
        Ok(inks) => inks,
        Err(e) => {
            log::warn!(
                "sidecar: failed to discover spot inks for page {}: {}; the \
                 transparency composite will proceed with no spot lanes",
                page_index,
                e
            );
            Vec::new()
        }
    }
}

/// Narrower variant of [`page_declares_transparency_or_overprint`]
/// that fires ONLY on transparency triggers (`/CA`, `/ca`, `/SMask`,
/// non-Normal `/BM`, `/Group`, XObject `/SMask`). Overprint flags
/// (`/OP`, `/op`) are intentionally NOT counted.
///
/// Used by the separation entry point to decide whether to route
/// through the composite-then-decompose path. The §11.4 transparency
/// model requires composite-first for correctness; the §11.7.4
/// overprint model is per-plate by definition (the per-plate walker
/// already implements OPM=0 / OPM=1 correctly), so routing pure-OP
/// pages through the composite path would either produce wrong plate
/// values (the page renderer's overprint handler is RGB-composite-
/// oriented, not per-plate) or require duplicating overprint logic
/// in the sidecar mirror. Drawing the line at "transparency only"
/// keeps the seam clean: detection-OFF and OP-only pages stay on the
/// per-plate walker; pages that mix transparency with overprint go
/// through composite-then-decompose where the §11.4 model evaluates
/// against the composite buffer.
pub(crate) fn page_declares_transparency(doc: &PdfDocument, resources: &Object) -> bool {
    let mut visited: std::collections::HashSet<crate::object::ObjectRef> =
        std::collections::HashSet::new();
    resources_declare_transparency_or_overprint(doc, resources, &mut visited, 0, false)
}

fn ext_g_states_signal_transparency_only(
    doc: &PdfDocument,
    ext_g_states: &HashMap<String, Object>,
) -> bool {
    for state in ext_g_states.values() {
        let state_resolved = match doc.resolve_object(state) {
            Ok(o) => o,
            Err(_) => continue,
        };
        let Some(state_dict) = state_resolved.as_dict() else {
            continue;
        };
        for key in ["CA", "ca"] {
            if let Some(v_raw) = state_dict.get(key) {
                let v = doc.resolve_object(v_raw).unwrap_or_else(|_| v_raw.clone());
                let alpha = match v {
                    Object::Real(r) => r as f32,
                    Object::Integer(i) => i as f32,
                    _ => 1.0,
                };
                if alpha < 1.0 {
                    return true;
                }
            }
        }
        if let Some(smask_raw) = state_dict.get("SMask") {
            let smask = doc
                .resolve_object(smask_raw)
                .unwrap_or_else(|_| smask_raw.clone());
            if !matches!(&smask, Object::Name(n) if n == "None") {
                return true;
            }
        }
        if let Some(bm_raw) = state_dict.get("BM") {
            let bm = doc
                .resolve_object(bm_raw)
                .unwrap_or_else(|_| bm_raw.clone());
            if bm_is_non_normal(&bm) {
                return true;
            }
        }
    }
    false
}

/// Conservative detection: does this page declare any resource that
/// could drive transparency or overprint? Returns `true` when the
/// sidecar should be allocated for the page.
///
/// Detection criteria (matches the round-4 pre-pass):
///
///   * Any `ExtGState` in `/Resources/ExtGState` declares one of:
///     - `/OP true` or `/op true` (overprint)
///     - `/CA < 1.0` or `/ca < 1.0` (transparent paint)
///     - `/SMask` non-null (soft mask)
///     - `/BM` non-Normal (non-trivial blend mode)
///   * Any Form XObject in `/Resources/XObject` declares a `/Group`
///     dict (transparency group) or carries an `/SMask` entry.
///
/// The detection-OFF path is byte-identical to a sidecar-less render
/// because the sidecar-consuming helpers fall back to additive-clamp
/// inversion when the sidecar is `None`.
pub(crate) fn page_declares_transparency_or_overprint(
    doc: &PdfDocument,
    resources: &Object,
) -> bool {
    let mut visited: std::collections::HashSet<crate::object::ObjectRef> =
        std::collections::HashSet::new();
    resources_declare_transparency_or_overprint(doc, resources, &mut visited, 0, true)
}

/// Maximum form-XObject resource recursion depth used by the detection
/// helpers. Mirrors `MAX_FORM_XOBJECT_DEPTH` over in the renderer's
/// content-walker; bounds at well above any realistic legitimate
/// nesting so the depth cap is purely a backstop against adversarial
/// /Resources cycles that escape the `visited` set.
const MAX_DETECTION_RECURSION: u32 = 32;

fn resources_declare_transparency_or_overprint(
    doc: &PdfDocument,
    resources: &Object,
    visited: &mut std::collections::HashSet<crate::object::ObjectRef>,
    depth: u32,
    include_overprint: bool,
) -> bool {
    if depth >= MAX_DETECTION_RECURSION {
        return false;
    }
    let res_dict = match resources {
        Object::Dictionary(d) => d,
        _ => return false,
    };

    if let Some(ext_gs_obj) = res_dict.get("ExtGState") {
        if let Ok(ext_gs_resolved) = doc.resolve_object(ext_gs_obj) {
            if let Some(ext_g_states) = ext_gs_resolved.as_dict() {
                let hit = if include_overprint {
                    ext_g_states_signal_transparency(doc, ext_g_states)
                } else {
                    ext_g_states_signal_transparency_only(doc, ext_g_states)
                };
                if hit {
                    return true;
                }
            }
        }
    }

    if let Some(xobj_obj) = res_dict.get("XObject") {
        if let Ok(xobj_resolved) = doc.resolve_object(xobj_obj) {
            if let Some(xobj_dict) = xobj_resolved.as_dict() {
                for raw in xobj_dict.values() {
                    // Skip XObjects we've already inspected at this
                    // scope: indirect refs are deduplicated by
                    // ObjectRef. Inline streams cannot self-reference,
                    // so the visited set only meaningfully tracks
                    // refs.
                    if let Some(r) = raw.as_reference() {
                        if !visited.insert(r) {
                            continue;
                        }
                    }
                    let resolved = match doc.resolve_object(raw) {
                        Ok(o) => o,
                        Err(_) => continue,
                    };
                    let dict = match &resolved {
                        Object::Stream { dict, .. } => Some(dict),
                        _ => None,
                    };
                    let Some(dict) = dict else { continue };

                    // §11.4.5 Form XObject: declaring its own /Group
                    // dict — or carrying an /SMask entry — is a
                    // direct transparency trigger.
                    if dict.contains_key("Group") || dict.contains_key("SMask") {
                        return true;
                    }
                    // §11.4.5 + §11.6.5.2: a Form XObject may also
                    // declare its own /Resources/ExtGState whose
                    // entries drive transparency from inside the
                    // form. The renderer evaluates the form's content
                    // under those state entries (§8.10.1), so they
                    // must count toward sidecar allocation the same
                    // way the page-level ExtGState does. Recurse on
                    // the form's resources (or fall through to the
                    // parent's when /Resources is absent).
                    let form_res = match dict.get("Resources").map(|r| doc.resolve_object(r)) {
                        Some(Ok(o)) => o,
                        _ => continue,
                    };
                    if resources_declare_transparency_or_overprint(
                        doc,
                        &form_res,
                        visited,
                        depth + 1,
                        include_overprint,
                    ) {
                        return true;
                    }
                }
            }
        }
    }

    false
}

fn ext_g_states_signal_transparency(
    doc: &PdfDocument,
    ext_g_states: &HashMap<String, Object>,
) -> bool {
    for state in ext_g_states.values() {
        let state_resolved = match doc.resolve_object(state) {
            Ok(o) => o,
            Err(_) => continue,
        };
        let Some(state_dict) = state_resolved.as_dict() else {
            continue;
        };
        let op_true = state_dict
            .get("OP")
            .map(|o| {
                let resolved = doc.resolve_object(o).unwrap_or_else(|_| o.clone());
                matches!(resolved, Object::Boolean(true))
            })
            .unwrap_or(false);
        let op_lower_true = state_dict
            .get("op")
            .map(|o| {
                let resolved = doc.resolve_object(o).unwrap_or_else(|_| o.clone());
                matches!(resolved, Object::Boolean(true))
            })
            .unwrap_or(false);
        if op_true || op_lower_true {
            return true;
        }
        for key in ["CA", "ca"] {
            if let Some(v_raw) = state_dict.get(key) {
                let v = doc.resolve_object(v_raw).unwrap_or_else(|_| v_raw.clone());
                let alpha = match v {
                    Object::Real(r) => r as f32,
                    Object::Integer(i) => i as f32,
                    _ => 1.0,
                };
                if alpha < 1.0 {
                    return true;
                }
            }
        }
        if let Some(smask_raw) = state_dict.get("SMask") {
            let smask = doc
                .resolve_object(smask_raw)
                .unwrap_or_else(|_| smask_raw.clone());
            if !matches!(&smask, Object::Name(n) if n == "None") {
                return true;
            }
        }
        // ISO 32000-1 §11.3.5 + §11.6.3: `/BM` may be a name OR an
        // array of names. For an array, "the first name that names a
        // blend mode supported by the conforming reader shall be used".
        // An unrecognised name maps to /Normal per §11.6.3. Walk both
        // shapes; fire the detection trigger only when the resolved
        // mode is non-/Normal. The raw `/BM` may itself be an indirect
        // ref to a name / array, so resolve before classifying.
        if let Some(bm_raw) = state_dict.get("BM") {
            let bm = doc
                .resolve_object(bm_raw)
                .unwrap_or_else(|_| bm_raw.clone());
            if bm_is_non_normal(&bm) {
                return true;
            }
        }
    }
    false
}

/// Resolve a `/BM` entry to "is this a recognised non-Normal blend
/// mode?". Handles both the name and array forms per §11.3.5 +
/// §11.6.3: the array form picks the FIRST recognised name; the name
/// form is classified directly. Unrecognised names fall through to
/// /Normal per the §11.6.3 fallback.
fn bm_is_non_normal(bm: &Object) -> bool {
    match bm {
        Object::Name(name) => is_non_normal_mode(name),
        Object::Array(arr) => arr
            .iter()
            .filter_map(Object::as_name)
            .find(|name| is_recognised_mode(name))
            .map(is_non_normal_mode)
            .unwrap_or(false),
        _ => false,
    }
}

/// True when `name` is one of the standard blend-mode names ISO 32000-1
/// §11.3.5 enumerates (separable §11.3.5.2 or non-separable §11.3.5.3).
/// `/Normal` counts as recognised. Unknown names are NOT recognised and
/// trigger the §11.6.3 fallback at the call site.
pub(crate) fn is_recognised_mode(name: &str) -> bool {
    matches!(
        name,
        "Normal"
            | "Multiply"
            | "Screen"
            | "Overlay"
            | "Darken"
            | "Lighten"
            | "ColorDodge"
            | "ColorBurn"
            | "HardLight"
            | "SoftLight"
            | "Difference"
            | "Exclusion"
            | "Hue"
            | "Saturation"
            | "Color"
            | "Luminosity"
    )
}

/// True when `name` is a recognised non-/Normal blend mode. The
/// transparency trigger fires only on this set.
fn is_non_normal_mode(name: &str) -> bool {
    is_recognised_mode(name) && name != "Normal"
}

/// Evaluate the §11.3.5.2 separable blend function `B(c_b, c_s)` for
/// one component. The PDF spec defines colour components as additive
/// values in `[0, 1]`. For SUBTRACTIVE-tint sidecar lanes (CMYK, spot),
/// the call site converts subtractive tint `t` to additive `1 - t`
/// before evaluating, then converts back. This helper does not do that
/// conversion — it operates on whatever component representation the
/// caller passes in, per ISO 32000-1 §11.3.5.2 Table 136.
///
/// Returns `c_s` unchanged when `mode` is not recognised (the §11.6.3
/// "unknown name → Normal" fallback), and returns `c_s` for `/Normal`.
///
/// Non-separable modes (`/Hue`, `/Saturation`, `/Color`, `/Luminosity`)
/// return `c_s` here because they cannot be evaluated component-wise —
/// the caller must dispatch on the BlendModeClass and route non-sep
/// modes through the §11.3.5.3 RGB projection helper. Spot lanes never
/// reach the non-sep formulas under §11.7.4.2 (the BM is substituted
/// to /Normal before this function is called) so the spot mirror's
/// non-sep return is unreachable in practice.
pub(crate) fn separable_blend(mode: &str, c_b: f32, c_s: f32) -> f32 {
    // ISO 32000-1 §11.3.5.2 Table 136.
    let c_b = c_b.clamp(0.0, 1.0);
    let c_s = c_s.clamp(0.0, 1.0);
    match mode {
        "Normal" => c_s,
        "Multiply" => c_b * c_s,
        "Screen" => c_b + c_s - c_b * c_s,
        "Overlay" => {
            // HardLight(c_s, c_b) — symmetric swap per Table 136.
            hard_light_component(c_s, c_b)
        }
        "Darken" => c_b.min(c_s),
        "Lighten" => c_b.max(c_s),
        "ColorDodge" => {
            if c_s >= 1.0 {
                1.0
            } else {
                (c_b / (1.0 - c_s)).min(1.0)
            }
        }
        "ColorBurn" => {
            if c_s <= 0.0 {
                0.0
            } else {
                1.0 - ((1.0 - c_b) / c_s).min(1.0)
            }
        }
        "HardLight" => hard_light_component(c_b, c_s),
        "SoftLight" => soft_light_component(c_b, c_s),
        "Difference" => (c_b - c_s).abs(),
        "Exclusion" => c_b + c_s - 2.0 * c_b * c_s,
        // §11.6.3 fallback: unknown / non-separable names render as
        // /Normal at the call site after dispatch routing. Returning
        // c_s here matches that policy if a caller reaches us with an
        // unexpected name.
        _ => c_s,
    }
}

fn hard_light_component(c_b: f32, c_s: f32) -> f32 {
    if c_s <= 0.5 {
        // Multiply(c_b, 2*c_s)
        c_b * 2.0 * c_s
    } else {
        // Screen(c_b, 2*c_s - 1)
        let twin = 2.0 * c_s - 1.0;
        c_b + twin - c_b * twin
    }
}

fn soft_light_component(c_b: f32, c_s: f32) -> f32 {
    // §11.3.5.2 Table 136 SoftLight: piecewise on c_s.
    if c_s <= 0.5 {
        c_b - (1.0 - 2.0 * c_s) * c_b * (1.0 - c_b)
    } else {
        let d = if c_b <= 0.25 {
            ((16.0 * c_b - 12.0) * c_b + 4.0) * c_b
        } else {
            c_b.sqrt()
        };
        c_b + (2.0 * c_s - 1.0) * (d - c_b)
    }
}
