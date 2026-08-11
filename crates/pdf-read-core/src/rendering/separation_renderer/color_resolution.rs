use super::*;

/// Resolve a colour-space name to a known classification.
///
/// Handles ISO 32000-1 §8.6.5.6: when the named space is one of the
/// Device families and the resource dictionary defines a corresponding
/// `Default*` entry, the Default mapping is consulted instead.
pub(super) fn resolve_color_space(
    space_name: &str,
    color_spaces: &HashMap<String, Object>,
    resources: &Object,
    doc: &PdfDocument,
) -> ResolvedSpace {
    // Direct Device* names — try DefaultCMYK / DefaultRGB / DefaultGray remap first.
    let default_key = match space_name {
        "DeviceCMYK" | "CMYK" => Some("DefaultCMYK"),
        "DeviceRGB" | "RGB" => Some("DefaultRGB"),
        "DeviceGray" | "G" => Some("DefaultGray"),
        _ => None,
    };
    if let Some(key) = default_key {
        if let Some(default) = color_spaces.get(key) {
            // Walk into the default array as a fresh classification.
            return classify_resolved(default, color_spaces, resources, doc);
        }
        return match key {
            "DefaultCMYK" => ResolvedSpace::Cmyk,
            "DefaultRGB" => ResolvedSpace::Rgb,
            _ => ResolvedSpace::Gray,
        };
    }

    if let Some(cs_obj) = color_spaces.get(space_name) {
        classify_resolved(cs_obj, color_spaces, resources, doc)
    } else {
        ResolvedSpace::Unknown
    }
}

/// Classify a colour-space object (either an array or a name) into a
/// [`ResolvedSpace`]. Used both as the entry point from a resource-dict
/// lookup and recursively when an array starts with a name that is
/// itself a device alias.
pub(super) fn classify_resolved(
    cs_obj: &Object,
    color_spaces: &HashMap<String, Object>,
    resources: &Object,
    doc: &PdfDocument,
) -> ResolvedSpace {
    // Plain name (e.g. /DeviceCMYK as the array's tail target).
    if let Some(name) = cs_obj.as_name() {
        return match name {
            "DeviceCMYK" | "CMYK" => ResolvedSpace::Cmyk,
            "DeviceRGB" | "RGB" => ResolvedSpace::Rgb,
            "DeviceGray" | "G" => ResolvedSpace::Gray,
            _ => resolve_color_space(name, color_spaces, resources, doc),
        };
    }

    let arr = match cs_obj.as_array() {
        Some(a) => a,
        None => return ResolvedSpace::Unknown,
    };
    let type_name = match arr.first().and_then(|o| o.as_name()) {
        Some(n) => n,
        None => return ResolvedSpace::Unknown,
    };
    match type_name {
        "DeviceCMYK" | "CMYK" => ResolvedSpace::Cmyk,
        "DeviceRGB" | "RGB" => ResolvedSpace::Rgb,
        "DeviceGray" | "G" => ResolvedSpace::Gray,
        "Pattern" => {
            // ISO 32000-1 §8.7.3.1: Pattern colour space's optional
            // index-1 element is the underlying colour space
            // (uncoloured Tiling carries the underlying space's
            // tints). For separation-ink scanning, recurse so a
            // Pattern[/Separation /Foo] marks /Foo as referenced.
            // Brings this into parity with the sidecar extractor's
            // Pattern arm.
            //
            // Real-world PDFs commonly share a Pattern's underlying
            // colour space via an indirect reference
            // (`/Pattern [<obj> <gen> R]`). Dereference before
            // recursing so the indirect form classifies identically
            // to an inline-array underlying. The sidecar's analogous
            // arm performs the same deref via its `deref` closure.
            match arr.get(1) {
                Some(underlying) => {
                    let resolved = doc
                        .resolve_object(underlying)
                        .unwrap_or_else(|_| underlying.clone());
                    classify_resolved(&resolved, color_spaces, resources, doc)
                }
                None => ResolvedSpace::Unknown,
            }
        }
        "Separation" => {
            let ink = arr
                .get(1)
                .and_then(|o| o.as_name())
                .map(|s| s.to_string())
                .unwrap_or_default();
            ResolvedSpace::Separation(ink)
        }
        "DeviceN" => {
            if let Some(Object::Array(ink_names)) = arr.get(1) {
                let names = ink_names
                    .iter()
                    .filter_map(|o| o.as_name().map(|s| s.to_string()))
                    .collect();
                ResolvedSpace::DeviceN(names)
            } else {
                ResolvedSpace::Unknown
            }
        }
        "ICCBased" => {
            // ICCBased: read /N from the stream dict to pick the component-count
            // interpretation. Unknown / unreachable / unsupported N → Unknown,
            // since fabricating CMYK plate values from an N=2 or N=5 profile
            // would silently corrupt output. tint_for_ink skips Unknown spaces.
            if let Some(stream_obj) = arr.get(1) {
                if let Ok(resolved) = doc.resolve_object(stream_obj) {
                    if let Object::Stream { ref dict, .. } = resolved {
                        if let Some(n) = dict.get("N").and_then(|o| o.as_integer()) {
                            return match n {
                                4 => ResolvedSpace::IccCmyk,
                                3 => ResolvedSpace::IccRgb,
                                1 => ResolvedSpace::IccGray,
                                _ => ResolvedSpace::Unknown,
                            };
                        }
                    }
                }
            }
            ResolvedSpace::Unknown
        }
        _ => ResolvedSpace::Unknown,
    }
}

/// Load color space definitions from page resources.
pub(super) fn load_color_spaces(
    doc: &PdfDocument,
    resources: &Object,
) -> Result<HashMap<String, Object>> {
    let mut color_spaces = HashMap::new();
    if let Object::Dictionary(res_dict) = resources {
        if let Some(cs_obj) = res_dict.get("ColorSpace") {
            let cs_dict_obj = doc.resolve_object(cs_obj)?;
            if let Some(cs_dict) = cs_dict_obj.as_dict() {
                for (name, o) in cs_dict {
                    if let Ok(resolved_cs) = doc.resolve_object(o) {
                        color_spaces.insert(name.clone(), resolved_cs);
                    }
                }
            }
        }
    }
    Ok(color_spaces)
}

/// Load font resources for the page. Failures are swallowed (text using
/// unloadable fonts is dropped); this matches the page renderer's
/// best-effort behaviour and keeps separation rendering robust on PDFs
/// with corrupt or missing fonts.
pub(super) fn load_fonts(doc: &PdfDocument, resources: &Object) -> HashMap<String, Arc<FontInfo>> {
    let mut fonts = HashMap::new();
    if let Object::Dictionary(res_dict) = resources {
        if let Some(font_obj) = res_dict.get("Font") {
            if let Ok(font_dict_obj) = doc.resolve_object(font_obj) {
                if let Some(font_dict) = font_dict_obj.as_dict() {
                    for (name, f_obj) in font_dict {
                        if let Ok(info) = doc.get_or_load_font_for_rendering(f_obj) {
                            fonts.insert(name.clone(), info);
                        }
                    }
                }
            }
        }
    }
    fonts
}

/// Decide how the current paint operation contributes to `target_ink`,
/// honoring ISO 32000-1 §11.7.4 (Overprint Control).
///
/// The decision tree:
///
/// ```text
/// For each plate P, source colour space S with component vector c[]:
///
///   if S = Separation(/All):                              Paint(c[0])
///   if S = Separation(/None) or empty components:         Skip
///   if S = Separation(name) and name == P:                Paint(c[0])
///   if S = Separation(name) and name != P:
///         overprint? Skip : Paint(0.0)                    // §11.7.4 default knockout
///
///   if S = DeviceN(names) and P in names:                 Paint(c[index_of_P])
///   if S = DeviceN(names) and P not in names:
///         overprint? Skip : Paint(0.0)
///
///   if S = DeviceCMYK / IccCmyk:
///         if P in {C, M, Y, K}:
///             overprint && opm == 1 && tint == 0.0 ? Skip : Paint(tint)
///         else:                                            // spot plate
///             overprint? Skip : Paint(0.0)                 // §11.7.4 default knockout
///
///   if S = RGB/Gray/IccRgb/IccGray:                       Skip
/// ```
pub(super) fn tint_for_ink(
    fill: bool,
    gs: &GraphicsState,
    color_spaces: &HashMap<String, Object>,
    resources: &Object,
    doc: &PdfDocument,
    target_ink: &str,
    fill_components: &[f32],
    stroke_components: &[f32],
) -> PaintAction {
    let space_name = if fill {
        &gs.fill_color_space
    } else {
        &gs.stroke_color_space
    };
    let components = if fill {
        fill_components
    } else {
        stroke_components
    };
    let overprint = if fill {
        gs.fill_overprint
    } else {
        gs.stroke_overprint
    };
    // §11.7.4.3: OPM applies only when the source is DeviceCMYK (or implicit
    // conversion thereto). The match arms below check this where relevant.
    let opm = gs.overprint_mode;

    // Default action when the source colour space doesn't name the
    // target plate: under OP=true, leave it alone; under OP=false (the
    // spec default), erase it to 0.0 ("areas of unspecified colorants
    // are erased" — §11.7.4).
    let other_plate_action = if overprint {
        PaintAction::Skip
    } else {
        PaintAction::Paint(0.0)
    };

    let resolved = resolve_color_space(space_name, color_spaces, resources, doc);
    match resolved {
        ResolvedSpace::Cmyk | ResolvedSpace::IccCmyk => {
            let cmyk_state = if fill {
                gs.fill_color_cmyk
            } else {
                gs.stroke_color_cmyk
            };
            let (c, m, y, k) = if let Some(v) = cmyk_state {
                v
            } else if components.len() >= 4 {
                (components[0], components[1], components[2], components[3])
            } else {
                return PaintAction::Skip;
            };
            let tint = match target_ink {
                "Cyan" => c,
                "Magenta" => m,
                "Yellow" => y,
                "Black" => k,
                // Spot plate — not in DeviceCMYK's colorant set.
                _ => return other_plate_action,
            };
            // §11.7.4 OPM=1 nonzero overprint: zero source components on
            // DeviceCMYK are treated as "not specified" — leave the
            // matching plate untouched. OPM=0 (default) paints zero,
            // which erases (knocks out) the plate.
            if overprint && opm == 1 && tint == 0.0 {
                PaintAction::Skip
            } else {
                PaintAction::Paint(tint)
            }
        }
        ResolvedSpace::Rgb
        | ResolvedSpace::Gray
        | ResolvedSpace::IccRgb
        | ResolvedSpace::IccGray => {
            // §11.7.4: overprint is a separation-space concept. RGB / Gray
            // sources do not route to ink plates at all. Converting them
            // would require a tint transform and is intentionally not done.
            PaintAction::Skip
        }
        ResolvedSpace::Separation(ink) => {
            // §8.6.6.4: /All paints to every plate; /None paints nothing.
            if components.is_empty() || ink == "None" {
                return PaintAction::Skip;
            }
            if ink == "All" {
                return PaintAction::Paint(components[0]);
            }
            if ink == target_ink {
                PaintAction::Paint(components[0])
            } else {
                other_plate_action
            }
        }
        ResolvedSpace::DeviceN(names) => {
            for (i, n) in names.iter().enumerate() {
                if n == "None" {
                    continue;
                }
                if (n == "All" || n == target_ink) && i < components.len() {
                    return PaintAction::Paint(components[i]);
                }
            }
            other_plate_action
        }
        ResolvedSpace::Unknown => PaintAction::Skip,
    }
}

/// Build a [`LogicalColor`] for the per-plate path from the current
/// graphics-state colour space and component values. Mirrors the
/// resolution the composite-side `build_logical_color` does, but
/// keyed on the separation walker's `gs.fill_color_space` /
/// `gs.stroke_color_space` strings and the parallel
/// `SeparationColorState` components vectors.
///
/// Returns `None` when the colour space can't be resolved or is empty.
pub(super) fn logical_color_for_side<'a>(
    fill: bool,
    gs: &'a GraphicsState,
    cs: &'a SeparationColorState,
    color_spaces: &'a HashMap<String, Object>,
) -> Option<LogicalColor<'a>> {
    let space_name = if fill {
        &gs.fill_color_space
    } else {
        &gs.stroke_color_space
    };
    let components = if fill {
        &cs.fill_components
    } else {
        &cs.stroke_components
    };
    let cmyk_state = if fill {
        gs.fill_color_cmyk
    } else {
        gs.stroke_color_cmyk
    };

    // Device-family aliases: emit the operator-side LogicalColor::Device
    // so the resolver passes straight through to the right channel
    // decomposition.
    match space_name.as_str() {
        "DeviceCMYK" | "CMYK" => {
            let (c, m, y, k) = cmyk_state.or_else(|| {
                if components.len() >= 4 {
                    Some((components[0], components[1], components[2], components[3]))
                } else {
                    None
                }
            })?;
            return Some(LogicalColor::Device(DeviceColor::Cmyk(c, m, y, k)));
        }
        "DeviceRGB" | "RGB" => {
            if components.len() >= 3 {
                return Some(LogicalColor::Device(DeviceColor::Rgb(
                    components[0],
                    components[1],
                    components[2],
                )));
            }
            return None;
        }
        "DeviceGray" | "G" => {
            if !components.is_empty() {
                return Some(LogicalColor::Device(DeviceColor::Gray(components[0])));
            }
            return None;
        }
        _ => {}
    }

    // Spaced: needs a borrow into the page-resource colour-space map.
    let space = color_spaces.get(space_name)?;
    let comps: SmallVec<[f32; 8]> = components.iter().copied().collect();
    Some(LogicalColor::Spaced {
        space,
        components: comps,
    })
}
