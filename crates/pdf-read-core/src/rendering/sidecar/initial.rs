use super::*;

/// Initial colour values for a colour space per ISO 32000-1 §8.6.8
/// ("The `CS`/`cs` operator shall also set the current colour to its
/// initial value, which depends on the colour space").
///
/// Carries every field a paint operator's downstream state cares about:
/// the raw component vector, the derived RGB triple (used by the
/// rasteriser for the default colour fallback), and the spot-ink
/// identity (Separation / non-process DeviceN).
pub(crate) struct InitialColour {
    /// The §8.6.8 component vector for the new space.
    pub components: Vec<f32>,
    /// The derived (r, g, b) triple stored on `fill_color_rgb` /
    /// `stroke_color_rgb` so the rasteriser has a default RGB even
    /// before an explicit `scn` lands.
    pub rgb: (f32, f32, f32),
    /// `Some(cmyk)` only when the new space is DeviceCMYK; cleared
    /// otherwise so a stale prior CMYK identity does not leak into
    /// overprint / compose-first paths.
    pub cmyk: Option<(f32, f32, f32, f32)>,
    /// Spot identity for the new space. For /Separation this is a
    /// single entry at the spec's initial tint 1.0; for non-process
    /// /DeviceN it is one entry per non-process colorant, each at
    /// tint 1.0. Every other space clears the spot identity.
    pub spot_inks: Vec<(String, f32)>,
}

/// Compute the per-§8.6.8 initial colour state for the colour space
/// named `space_name`. `resolved_space` is the `Object` resolved from
/// the page's `/Resources/ColorSpace` subdictionary (or `None` for the
/// inline device-family names DeviceGray / DeviceRGB / DeviceCMYK /
/// Pattern that never appear in the resource dict).
///
/// Spec text (ISO 32000-1 §8.6.8):
///  - DeviceGray / CalGray / Indexed: 0.0
///  - DeviceRGB / CalRGB / Lab: (0, 0, 0)
///  - DeviceCMYK: (0, 0, 0, 1)  (pure black)
///  - ICCBased: all-zeros unless clamped to /Range
///  - Separation: tint 1.0  (§8.6.6.4 explicitly: "The initial value
///    for both the stroking and nonstroking colour in the graphics
///    state shall be 1.0.")
///  - DeviceN: tint 1.0 per colorant
///  - Pattern: a nothing-painted pattern object (we represent this as
///    an empty component vector — the rasteriser already treats the
///    Pattern space as "no fill" until an `scn` lands)
pub(crate) fn initial_colour_for_space(
    space_name: &str,
    resolved_space: Option<&Object>,
    doc: &PdfDocument,
    rendering_intent: crate::color::RenderingIntent,
    retarget_cache: Option<&crate::rendering::resolution::context::IccTransformCache>,
) -> InitialColour {
    let deref =
        |obj: &Object| -> Object { doc.resolve_object(obj).unwrap_or_else(|_| obj.clone()) };

    // Device-family direct names (no array form).
    match space_name {
        "DeviceGray" | "G" | "CalGray" => {
            return InitialColour {
                components: vec![0.0],
                rgb: (0.0, 0.0, 0.0),
                cmyk: None,
                spot_inks: Vec::new(),
            };
        }
        "DeviceRGB" | "RGB" | "CalRGB" => {
            return InitialColour {
                components: vec![0.0, 0.0, 0.0],
                rgb: (0.0, 0.0, 0.0),
                cmyk: None,
                spot_inks: Vec::new(),
            };
        }
        "DeviceCMYK" | "CMYK" => {
            // Initial CMYK is (0, 0, 0, 1) — pure black per §8.6.8.
            let (r, g, b) = (0.0_f32, 0.0_f32, 0.0_f32);
            return InitialColour {
                components: vec![0.0, 0.0, 0.0, 1.0],
                rgb: (r, g, b),
                cmyk: Some((0.0, 0.0, 0.0, 1.0)),
                spot_inks: Vec::new(),
            };
        }
        "Pattern" => {
            return InitialColour {
                components: Vec::new(),
                rgb: (0.0, 0.0, 0.0),
                cmyk: None,
                spot_inks: Vec::new(),
            };
        }
        _ => {}
    }

    // Resource-defined space: inspect the array form.
    let arr = match resolved_space.and_then(Object::as_array) {
        Some(a) => a,
        None => {
            return InitialColour {
                components: Vec::new(),
                rgb: (0.0, 0.0, 0.0),
                cmyk: None,
                spot_inks: Vec::new(),
            };
        }
    };
    let type_name = arr.first().and_then(Object::as_name).unwrap_or("");
    match type_name {
        "CalGray" => InitialColour {
            components: vec![0.0],
            rgb: (0.0, 0.0, 0.0),
            cmyk: None,
            spot_inks: Vec::new(),
        },
        "CalRGB" | "Lab" => InitialColour {
            components: vec![0.0, 0.0, 0.0],
            rgb: (0.0, 0.0, 0.0),
            cmyk: None,
            spot_inks: Vec::new(),
        },
        "ICCBased" => {
            let n = arr
                .get(1)
                .map(deref)
                .as_ref()
                .and_then(Object::as_dict)
                .and_then(|d| d.get("N"))
                .and_then(Object::as_integer)
                .unwrap_or(3);
            // §8.6.8: ICCBased initial colour is all-zeros unless the
            // /Range entry clamps. We assume 0.0 is in-range (the
            // common case); a custom /Range that excludes 0 is rare
            // and the rasteriser will clamp downstream anyway.
            let components = vec![0.0_f32; n.max(1) as usize];
            let cmyk = if n == 4 {
                Some((0.0, 0.0, 0.0, 0.0))
            } else {
                None
            };
            InitialColour {
                components,
                rgb: (0.0, 0.0, 0.0),
                cmyk,
                spot_inks: Vec::new(),
            }
        }
        "Indexed" => InitialColour {
            components: vec![0.0],
            rgb: (0.0, 0.0, 0.0),
            cmyk: None,
            spot_inks: Vec::new(),
        },
        "Separation" => {
            // §8.6.8 + §8.6.6.4: initial tint 1.0 for the colorant.
            let name_obj = arr.get(1).map(deref);
            let ink = name_obj
                .as_ref()
                .and_then(Object::as_name)
                .map(str::to_string)
                .unwrap_or_default();
            let spot_inks = if !ink.is_empty() && ink != "All" && ink != "None" {
                vec![(ink, 1.0)]
            } else {
                // /All and /None branch in §8.6.6.3; both are handled
                // at paint time, not via spot identity.
                Vec::new()
            };
            InitialColour {
                components: vec![1.0],
                rgb: (0.0, 0.0, 0.0),
                cmyk: None,
                spot_inks,
            }
        }
        "DeviceN" => {
            let names_obj = arr.get(1).map(deref);
            let names = names_obj
                .as_ref()
                .and_then(Object::as_array)
                .map(|names| {
                    names
                        .iter()
                        .filter_map(|o| o.as_name().map(str::to_string))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            // Filter /Process channels from the spot set, same as the
            // paint-time extractor does.
            let process_names: std::collections::HashSet<String> = arr
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
                        .collect()
                })
                .unwrap_or_default();
            let spot_inks: Vec<(String, f32)> = names
                .iter()
                .filter(|n| n.as_str() != "All" && n.as_str() != "None")
                .filter(|n| !process_names.contains(*n))
                .map(|n| (n.clone(), 1.0_f32))
                .collect();
            // §8.6.8: initial tint is 1.0 for every colorant.
            let components = vec![1.0_f32; names.len().max(1)];
            // §8.6.6.5 + §11.7.4.3: when /Process attribution is
            // declared, the initial-tint vector feeds the process
            // /ColorSpace exactly like an `scn` would. The overprint
            // dispatcher reads `cmyk` for the §11.7.4.3 source CMYK;
            // without this population the initial-colour CMYK would
            // be lost (the call site would fall through to the
            // §10.3.5 RGB inverse from `fill_color_rgb = (0,0,0)`,
            // producing source CMYK (1, 1, 1, 0) — K dropped). Run
            // the same evaluator the paint-time path uses so the
            // mapping (named device families + ICCBased N=1/3/4) is
            // identical to the post-`scn` behaviour.
            let cmyk = extract_process_paint_cmyk(
                resolved_space.unwrap(),
                &components,
                doc,
                rendering_intent,
                retarget_cache,
            );
            InitialColour {
                components,
                rgb: (0.0, 0.0, 0.0),
                cmyk,
                spot_inks,
            }
        }
        "Pattern" => InitialColour {
            components: Vec::new(),
            rgb: (0.0, 0.0, 0.0),
            cmyk: None,
            spot_inks: Vec::new(),
        },
        _ => InitialColour {
            components: Vec::new(),
            rgb: (0.0, 0.0, 0.0),
            cmyk: None,
            spot_inks: Vec::new(),
        },
    }
}
