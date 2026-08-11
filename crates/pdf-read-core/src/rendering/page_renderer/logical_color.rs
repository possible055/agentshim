use super::*;

/// Returns `true` when the operator paints pixels into the pixmap.
///
/// Used by the knockout-group renderer to segment the operator stream
/// at element boundaries. Per ISO 32000-1:2008 §11.4.6.2 each "element"
/// in a knockout group is delimited by a paint operator and composites
/// independently against the group's initial backdrop.
pub(super) fn is_paint_operator(op: &Operator) -> bool {
    matches!(
        op,
        Operator::Fill
            | Operator::FillEvenOdd
            | Operator::Stroke
            | Operator::FillStroke
            | Operator::FillStrokeEvenOdd
            | Operator::CloseFillStroke
            | Operator::CloseFillStrokeEvenOdd
            | Operator::PaintShading { .. }
            | Operator::Do { .. }
            | Operator::InlineImage { .. }
            | Operator::Tj { .. }
            | Operator::TJ { .. }
            | Operator::Quote { .. }
            | Operator::DoubleQuote { .. }
    )
}

/// Returns `true` when the resolved `(r, g, b, a)` matches the supplied
/// rgb triple and alpha within [`RGBA_MATCH_EPSILON`] on every channel.
///
/// Used by the resolution-pipeline helpers to detect no-op overrides:
/// for Device-family inputs the pipeline always produces an RGBA, but
/// the value is the same one the inline path would have read from
/// `gs.*_color_rgb` directly. Skipping the splice in that case keeps
/// the resolution path allocation-free for the common case where no
/// Separation/DeviceN colour space is in play.
pub(super) fn rgba_matches(
    resolved: (f32, f32, f32, f32),
    rgb: (f32, f32, f32),
    alpha: f32,
) -> bool {
    let (r, g, b, a) = resolved;
    let (gr, gg, gb) = rgb;
    (r - gr).abs() <= RGBA_MATCH_EPSILON
        && (g - gg).abs() <= RGBA_MATCH_EPSILON
        && (b - gb).abs() <= RGBA_MATCH_EPSILON
        && (a - alpha).abs() <= RGBA_MATCH_EPSILON
}

/// Build a [`LogicalColor`] from the dispatcher's view of the active colour:
/// the fill colour space name, the raw components on the stack, and (when the
/// space is non-Device) the resolved space object from the resources map.
pub(super) fn build_logical_color<'a>(
    space_name: &str,
    components: &[f32],
    resolved_space: Option<&'a Object>,
) -> LogicalColor<'a> {
    // Device families fold directly into `LogicalColor::Device` — the
    // resolver's spec-conformance for these is verified by colour-stage
    // unit tests; routing through the same Device path keeps the
    // pipeline's behaviour identical to the inline path for the
    // non-Separation cases.
    //
    // Component-count mismatch (e.g. `/ColorSpace /DeviceCMYK` with only
    // 1 component on the stack) falls through to the `_ =>` arm below,
    // which routes through the resolver's gray fallback. Output happens
    // to match the inline `parse_color_array` single-element-array
    // expansion `(g, g, g)` — both paths paint the gray value across
    // all three RGB channels.
    match space_name {
        "DeviceGray" | "G" if !components.is_empty() => {
            LogicalColor::Device(DeviceColor::Gray(components[0]))
        }
        "DeviceRGB" | "RGB" if components.len() >= 3 => LogicalColor::Device(DeviceColor::Rgb(
            components[0],
            components[1],
            components[2],
        )),
        "DeviceCMYK" | "CMYK" if components.len() >= 4 => LogicalColor::Device(DeviceColor::Cmyk(
            components[0],
            components[1],
            components[2],
            components[3],
        )),
        _ => {
            // Non-device space: hand the resolver the space object so it
            // can dispatch on Separation / DeviceN / ICCBased / Indexed.
            // Fall back to `DeviceGray` as a logical-colour shape if the
            // resources map didn't carry an entry for this name — the
            // resolver's gray fallback then matches the inline path.
            //
            // Use a thread-local static name object to satisfy the
            // `'a` lifetime on the fallback arm without cloning.
            use std::sync::OnceLock;
            static GRAY_FALLBACK: OnceLock<Object> = OnceLock::new();
            let space = resolved_space.unwrap_or_else(|| {
                GRAY_FALLBACK.get_or_init(|| Object::Name("DeviceGray".to_string()))
            });
            LogicalColor::Spaced {
                space,
                components: components.iter().copied().collect(),
            }
        }
    }
}

/// Resolve the named ExtGState entry from `resources` and parse the fields we
/// need. Kept as a thin wrapper that re-resolves the resource dict per call —
/// the hot path in `execute_operators` uses `parse_ext_g_state_inner` against
/// a pre-resolved resource dict (the per-form ExtGState dict has 10 000+
/// entries on heavy vector figures and deep-cloning it on every `gs` op was
/// the previous bottleneck).
pub(super) fn parse_ext_g_state(
    dict_name: &str,
    resources: &Object,
    doc: &PdfDocument,
) -> Result<ParsedExtGState> {
    let out = ParsedExtGState::default();
    let res_dict = match resources {
        Object::Dictionary(d) => d,
        _ => return Ok(out),
    };
    let ext_gs_obj = match res_dict.get("ExtGState") {
        Some(o) => o,
        None => return Ok(out),
    };
    let ext_gs_resolved = doc.resolve_object(ext_gs_obj)?;
    let ext_g_states = match ext_gs_resolved.as_dict() {
        Some(d) => d,
        None => return Ok(out),
    };
    let state_obj = match ext_g_states.get(dict_name) {
        Some(o) => o,
        None => return Ok(out),
    };
    parse_ext_g_state_inner(state_obj, doc)
}
