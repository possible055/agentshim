use super::*;

// Test-only counter that records how many `apply_pending_clip` calls actually
// materialized a clip (i.e. did real rasterization work). Used by the
// regression probe below to lock in the per-paint-op fast path.
#[cfg(test)]
pub(crate) static APC_MATERIALIZED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// ISO 32000-1 §11.7.4.3 / Table 149 source colour space classes.
///
/// The CompatibleOverprint blend function `B(c_b, c_s)` selects between
/// source replace (`c_s`) and backdrop preserve (`c_b`) per-channel
/// based on (a) which source CS class the paint operator uses and (b)
/// whether OPM=1's zero-source-preserve rule applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SourceCsClass {
    /// `DeviceCMYK` specified directly via `k` / `K` / `sc` / `scn` on
    /// a `/DeviceCMYK` colour space. This is Table 149 row 1 — the only
    /// class for which the OPM=1 zero-source-preserve rule applies. The
    /// process colour components (C, M, Y, K) of the group colour space
    /// receive `B = c_s` under OPM=0 and `B = (c_s if c_s≠0 else c_b)`
    /// under OPM=1.
    DeviceCmykDirect,
    /// Any other process colour space — `DeviceGray`, `DeviceRGB`,
    /// `CalGray`, `CalRGB`, `ICCBased` of any N, or `DeviceCMYK`
    /// not-directly-specified (e.g. a sampled image's pixel colours).
    /// Table 149 row 2: all process colour components of the group CS
    /// get `B = c_s` regardless of OPM. The OPM=1 zero-source-preserve
    /// rule does not apply (§11.7.4.5: "Nonzero overprint mode shall
    /// apply only to painting operations that use the current colour
    /// in the graphics state when the current colour space is
    /// DeviceCMYK").
    OtherProcess,
    /// `Separation` or non-process `DeviceN`. Table 149 row 3: process
    /// colour components preserve backdrop (`B = c_b`); the named-spot
    /// lanes carry `c_s`; unnamed spot lanes preserve backdrop. The
    /// process-side override is the dispositive difference from the
    /// process-CS classes — a Separation paint must NOT mark process
    /// plates even when its alternate colour space rasterised an RGB
    /// approximation into the composite buffer.
    SeparationOrDeviceN,
}

/// One of the four DeviceCMYK process channels. Used by
/// [`compose_overprint_channel`] to identify which channel index of the
/// `Source` CMYK quadruple a per-channel call concerns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProcessChannel {
    C,
    M,
    Y,
    K,
}

/// Resolved source colour for the §11.7.4.3 CompatibleOverprint path.
///
/// The CMYK quadruple is the source colour expressed in DeviceCMYK
/// regardless of the original colour space — for DeviceGray it is
/// `(0, 0, 0, 1-g)`, for DeviceRGB it is the §10.3.5 additive-clamp
/// inverse, and for Separation/DeviceN it is the alternate-space
/// evaluation (or `(0, 0, 0, 0)` when the alternate path produces
/// nothing — in that case the process-lane preserve rule does the work).
#[derive(Debug, Clone, Copy)]
pub(super) struct OverprintSource {
    pub(super) class: SourceCsClass,
    pub(super) cmyk: (f32, f32, f32, f32),
}

/// Determine the §11.7.4.3 source colour for an overprint paint.
///
/// Returns `None` when no `B(c_b, c_s)` would fire — the caller should
/// skip the per-channel pass.
///
/// The dispatch reads `gs.fill_color_space` / `gs.stroke_color_space`
/// to classify the source. For DeviceCMYK direct we also require
/// `fill_color_cmyk` / `stroke_color_cmyk` populated; if it is missing
/// (e.g. a stale state where the colour space name is "DeviceCMYK" but
/// the components vector is empty) we degrade gracefully to
/// `OtherProcess` so the source CMYK is recovered from the RGB
/// fallback below.
pub(super) fn source_for_overprint(gs: &GraphicsState, fill_side: bool) -> Option<OverprintSource> {
    let (space_name, color_cmyk, color_rgb, components, spot_inks) = if fill_side {
        (
            gs.fill_color_space.as_str(),
            gs.fill_color_cmyk,
            gs.fill_color_rgb,
            &gs.fill_color_components,
            &gs.fill_spot_inks,
        )
    } else {
        (
            gs.stroke_color_space.as_str(),
            gs.stroke_color_cmyk,
            gs.stroke_color_rgb,
            &gs.stroke_color_components,
            &gs.stroke_spot_inks,
        )
    };
    let overprint_active = if fill_side {
        gs.fill_overprint
    } else {
        gs.stroke_overprint
    };
    if !overprint_active {
        return None;
    }

    match space_name {
        "DeviceCMYK" | "CMYK" => {
            // Table 149 row 1: DeviceCMYK specified directly. The
            // graphics-state CMYK quadruple is the source. When the
            // colour space is named DeviceCMYK but no component vector
            // landed yet (initial-colour edge case after a `cs` without
            // an `scn`), fall back to (0, 0, 0, 1) — the spec's §8.6.8
            // initial colour for DeviceCMYK.
            let cmyk = color_cmyk.unwrap_or((0.0, 0.0, 0.0, 1.0));
            Some(OverprintSource {
                class: SourceCsClass::DeviceCmykDirect,
                cmyk,
            })
        }
        "DeviceGray" | "G" | "CalGray" => {
            // Table 149 row 2: DeviceGray maps to CMYK as (0, 0, 0, 1-g)
            // per the standard gray→CMYK conversion (used by the
            // device-space paint pipeline and §10.3.5).
            let g = components.first().copied().unwrap_or(color_rgb.0);
            let k = (1.0 - g).clamp(0.0, 1.0);
            Some(OverprintSource {
                class: SourceCsClass::OtherProcess,
                cmyk: (0.0, 0.0, 0.0, k),
            })
        }
        "DeviceRGB" | "RGB" | "CalRGB" => {
            // Table 149 row 2: DeviceRGB maps to CMYK via the §10.3.5
            // additive-clamp inverse `C = 1 - R`, `M = 1 - G`,
            // `Y = 1 - B`, `K = 0`.
            let r = components.first().copied().unwrap_or(color_rgb.0);
            let g = components.get(1).copied().unwrap_or(color_rgb.1);
            let b = components.get(2).copied().unwrap_or(color_rgb.2);
            let c = (1.0 - r).clamp(0.0, 1.0);
            let m = (1.0 - g).clamp(0.0, 1.0);
            let y = (1.0 - b).clamp(0.0, 1.0);
            Some(OverprintSource {
                class: SourceCsClass::OtherProcess,
                cmyk: (c, m, y, 0.0),
            })
        }
        _ => {
            // Composite-named space — Separation, DeviceN, ICCBased,
            // Indexed, Pattern. The spot lanes (if any) are mirrored
            // separately by `mirror_spot_paint_into_sidecar_with_coverage`;
            // here we only need to know the process-side rule for the
            // four CMYK channels.
            //
            // Dispatch precedence:
            //
            // 1. `color_cmyk` populated — DeviceN /Process attribution
            //    (§8.6.6.5) is in play and the source CMYK was
            //    reconstructed in `SetFillColorN`. Process lanes follow
            //    Table 149 row 2 "any other process colour space"
            //    regardless of whether a spot tail is also present:
            //    the spot tail's tints land via the spot mirror, but
            //    the process tail's tints still drive the process
            //    channels via `B = c_s`. Mixed DeviceN /Process+spot
            //    must NOT preserve backdrop on the process lanes — the
            //    process tints are sourced from the same `scn` and
            //    contribute to the C/M/Y/K plates.
            //
            // 2. `spot_inks` non-empty (no process CMYK) — pure
            //    Separation or DeviceN with NO process attribution.
            //    Process lanes preserve backdrop per Table 149 row 3;
            //    the named spot lanes are handled by the spot mirror.
            //
            // 3. Otherwise — ICCBased / Pattern / Indexed / DeviceN
            //    /Process whose /Process /ColorSpace the dispatcher
            //    could not resolve (CalRGB / CalGray array forms,
            //    malformed /Components per
            //    HONEST_GAP_DEVICEN_PROCESS_MISMATCHED_NAMES). Falls
            //    under Table 149 row 2; recover CMYK from the
            //    convert-from-RGB additive-clamp inverse so the
            //    per-process-channel `B = c_s` rule has a defensible
            //    source value.
            if let Some(cmyk) = color_cmyk {
                Some(OverprintSource {
                    class: SourceCsClass::OtherProcess,
                    cmyk,
                })
            } else if !spot_inks.is_empty() {
                Some(OverprintSource {
                    class: SourceCsClass::SeparationOrDeviceN,
                    cmyk: (0.0, 0.0, 0.0, 0.0),
                })
            } else {
                let (r, g, b) = color_rgb;
                let c = (1.0 - r).clamp(0.0, 1.0);
                let m = (1.0 - g).clamp(0.0, 1.0);
                let y = (1.0 - b).clamp(0.0, 1.0);
                Some(OverprintSource {
                    class: SourceCsClass::OtherProcess,
                    cmyk: (c, m, y, 0.0),
                })
            }
        }
    }
}

/// ISO 32000-1 §11.7.4.3 + §11.3.3 per-channel composed result.
///
/// Computes `c_r = α · B(c_b, c_s) + (1 − α) · c_b` for one process
/// channel, where `B` is the CompatibleOverprint blend function per
/// Table 149. The dispatch closely follows Table 149's rows; see the
/// docstring on [`PageRenderer::apply_overprint_after_paint`] for the
/// table layout.
///
/// - `class` — which Table 149 row applies.
/// - `channel` — the C/M/Y/K identity of this call.
/// - `c_s`, `c_b` — source and backdrop subtractive tints for this
///   channel.
/// - `opm` — graphics-state `/OPM` value (0 or 1).
/// - `alpha` — effective shape × opacity for the pixel.
pub(super) fn compose_overprint_channel(
    class: SourceCsClass,
    _channel: ProcessChannel,
    c_s: f32,
    c_b: f32,
    opm: u8,
    alpha: f32,
) -> f32 {
    let b = match class {
        SourceCsClass::DeviceCmykDirect => {
            // Table 149 row 1: B = c_s for C/M/Y/K under OPM=0 or when
            // c_s ≠ 0 under OPM=1; B = c_b for c_s == 0 under OPM=1.
            // The §11.7.4.5 NOTE 1 explicitly restricts the OPM=1
            // preserve rule to the directly-specified-DeviceCMYK case.
            if opm == 1 && c_s == 0.0 {
                c_b
            } else {
                c_s
            }
        }
        SourceCsClass::OtherProcess => {
            // Table 149 row 2: B = c_s for every process colour
            // component of the group CS regardless of OPM.
            c_s
        }
        SourceCsClass::SeparationOrDeviceN => {
            // Table 149 row 3: process colour components preserve
            // backdrop. The named-spot lanes are handled by the spot
            // sidecar mirror, not by this per-process-channel pass.
            c_b
        }
    };
    let alpha = alpha.clamp(0.0, 1.0);
    alpha * b + (1.0 - alpha) * c_b
}
