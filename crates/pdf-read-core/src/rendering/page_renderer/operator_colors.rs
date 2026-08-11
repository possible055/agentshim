use super::*;

impl PageRenderer {
    pub(super) fn execute_color_operator(
        &mut self,
        op: &Operator,
        gs_stack: &mut GraphicsStateStack,
        doc: &PdfDocument,
    ) -> bool {
        match op {
            Operator::SetFillRgb { r, g, b } => {
                let gs = gs_stack.current_mut();
                gs.fill_color_rgb = (*r, *g, *b);
                gs.fill_color_space = "DeviceRGB".to_string();
                gs.fill_color_components.clear();
                gs.fill_color_components.extend_from_slice(&[*r, *g, *b]);
                // Device-family fill paint: per §11.7.3 the source
                // covers only the process channels, so any spot ink
                // identity recorded by a prior /Separation or
                // /DeviceN paint is no longer the active source.
                // The sidecar's per-paint spot mirror reads this
                // empty list as "no spot lane writes for this paint".
                gs.fill_spot_inks.clear();
                // ISO 32000-1 §8.6.3: the fill colour and colour
                // space are coupled — switching to /DeviceRGB
                // invalidates any prior /DeviceCMYK identity. Failing
                // to clear `fill_color_cmyk` here means the §11.7.4.3
                // overprint path would still see the prior paint's
                // CMYK quadruple as the "current source colour",
                // producing wrong B(c_b, c_s) = c_s values for the
                // new RGB paint's region.
                gs.fill_color_cmyk = None;
                log::debug!("SetFillRgb: [{}, {}, {}]", r, g, b);
            }
            Operator::SetStrokeRgb { r, g, b } => {
                let gs = gs_stack.current_mut();
                gs.stroke_color_rgb = (*r, *g, *b);
                gs.stroke_color_space = "DeviceRGB".to_string();
                gs.stroke_color_components.clear();
                gs.stroke_color_components.extend_from_slice(&[*r, *g, *b]);
                gs.stroke_spot_inks.clear();
                gs.stroke_color_cmyk = None;
                log::debug!("SetStrokeRgb: [{}, {}, {}]", r, g, b);
            }
            Operator::SetFillGray { gray } => {
                let g = *gray;
                let gs = gs_stack.current_mut();
                gs.fill_color_rgb = (g, g, g);
                gs.fill_color_space = "DeviceGray".to_string();
                gs.fill_color_components.clear();
                gs.fill_color_components.push(g);
                gs.fill_spot_inks.clear();
                gs.fill_color_cmyk = None;
                log::debug!("SetFillGray: {}", g);
            }
            Operator::SetStrokeGray { gray } => {
                let g = *gray;
                let gs = gs_stack.current_mut();
                gs.stroke_color_rgb = (g, g, g);
                gs.stroke_color_space = "DeviceGray".to_string();
                gs.stroke_color_components.clear();
                gs.stroke_color_components.push(g);
                gs.stroke_spot_inks.clear();
                gs.stroke_color_cmyk = None;
                log::debug!("SetStrokeGray: {}", g);
            }
            Operator::SetFillCmyk { c, m, y, k } => {
                // Convert CMYK to RGB
                let (r, g, b) = cmyk_to_rgb(*c, *m, *y, *k);
                let gs = gs_stack.current_mut();
                gs.fill_color_rgb = (r, g, b);
                gs.fill_color_cmyk = Some((*c, *m, *y, *k));
                gs.fill_color_space = "DeviceCMYK".to_string();
                gs.fill_color_components.clear();
                gs.fill_color_components
                    .extend_from_slice(&[*c, *m, *y, *k]);
                gs.fill_spot_inks.clear();
                log::debug!(
                    "SetFillCmyk: [{}, {}, {}, {}] -> {:?}",
                    c,
                    m,
                    y,
                    k,
                    (r, g, b)
                );
            }
            Operator::SetStrokeCmyk { c, m, y, k } => {
                let (r, g, b) = cmyk_to_rgb(*c, *m, *y, *k);
                let gs = gs_stack.current_mut();
                gs.stroke_color_rgb = (r, g, b);
                gs.stroke_color_cmyk = Some((*c, *m, *y, *k));
                gs.stroke_color_space = "DeviceCMYK".to_string();
                gs.stroke_color_components.clear();
                gs.stroke_color_components
                    .extend_from_slice(&[*c, *m, *y, *k]);
                gs.stroke_spot_inks.clear();
                log::debug!(
                    "SetStrokeCmyk: [{}, {}, {}, {}] -> {:?}",
                    c,
                    m,
                    y,
                    k,
                    (r, g, b)
                );
            }

            // Color space operators
            Operator::SetFillColorSpace { name } => {
                // ISO 32000-1 §8.6.8: the `cs` operator shall also
                // set the current colour to its initial value, which
                // depends on the colour space. For Separation /
                // DeviceN the initial tint is 1.0 per colorant
                // (§8.6.6.4 / §8.6.6.5); for DeviceCMYK the initial
                // colour is (0, 0, 0, 1); device-family RGB / Gray
                // start at all-zeros. Failing to reset the colour
                // here means a paint after `cs /CS_B` without an
                // intervening `scn` would carry the prior space's
                // identity and tint, including its spot ink list —
                // round 2 QA pinned that the spot mirror would then
                // write the prior /CS_A's spot lane.
                let resolved = self.color_spaces.get(name).cloned();
                // §10.7.3: the §8.6.8 initial-colour evaluation runs an
                // ICC retarget for DeviceN /Process /ICCBased; thread
                // the live gs intent through so a prior `/Perceptual ri`
                // / ExtGState /RI propagates into the retarget tag pick.
                let intent_for_initial = crate::color::RenderingIntent::from_pdf_name(
                    &gs_stack.current().rendering_intent,
                );
                let initial = sidecar_mod::initial_colour_for_space(
                    name,
                    resolved.as_ref(),
                    doc,
                    intent_for_initial,
                    Some(&self.icc_transform_cache),
                );
                let gs = gs_stack.current_mut();
                gs.fill_color_space = name.clone();
                gs.fill_color_rgb = initial.rgb;
                gs.fill_color_cmyk = initial.cmyk;
                gs.fill_color_components.clear();
                gs.fill_color_components
                    .extend_from_slice(&initial.components);
                gs.fill_spot_inks = initial.spot_inks;
                // Selecting a colour space clears any previously selected
                // fill pattern; a fresh scn must re-name it (§8.7.3).
                gs.fill_pattern_name = None;
                log::debug!("SetFillColorSpace: {}", name);
            }
            Operator::SetStrokeColorSpace { name } => {
                let resolved = self.color_spaces.get(name).cloned();
                let intent_for_initial = crate::color::RenderingIntent::from_pdf_name(
                    &gs_stack.current().rendering_intent,
                );
                let initial = sidecar_mod::initial_colour_for_space(
                    name,
                    resolved.as_ref(),
                    doc,
                    intent_for_initial,
                    Some(&self.icc_transform_cache),
                );
                let gs = gs_stack.current_mut();
                gs.stroke_color_space = name.clone();
                gs.stroke_color_rgb = initial.rgb;
                gs.stroke_color_cmyk = initial.cmyk;
                gs.stroke_color_components.clear();
                gs.stroke_color_components
                    .extend_from_slice(&initial.components);
                gs.stroke_spot_inks = initial.spot_inks;
            }
            Operator::SetFillColor { components } => {
                let gs = gs_stack.current_mut();
                let space_name = gs.fill_color_space.clone();
                let resolved_space = self.color_spaces.get(&space_name);
                gs.fill_color_components.clear();
                gs.fill_color_components.extend_from_slice(components);
                // ISO 32000-1 §8.6.3 + §11.7.4.3: `sc` mutates the
                // current fill colour for the active colour space.
                // Clear any stale CMYK identity left over from a
                // prior DeviceCMYK paint; the DeviceCMYK arm below
                // refills it. Without this clear, a SetFillColor on
                // a non-CMYK space leaves the prior CMYK quadruple
                // visible to the §11.7.4.3 overprint path and
                // corrupts the per-channel B(c_b, c_s) result.
                gs.fill_color_cmyk = None;

                match space_name.as_str() {
                    "DeviceGray" | "G" if !components.is_empty() => {
                        let g = components[0];
                        gs.fill_color_rgb = (g, g, g);
                    }
                    "DeviceRGB" | "RGB" if components.len() >= 3 => {
                        gs.fill_color_rgb = (components[0], components[1], components[2]);
                    }
                    "DeviceCMYK" | "CMYK" if components.len() >= 4 => {
                        gs.fill_color_rgb =
                            cmyk_to_rgb(components[0], components[1], components[2], components[3]);
                        gs.fill_color_cmyk =
                            Some((components[0], components[1], components[2], components[3]));
                    }
                    _ => {
                        let mut handled = false;
                        if let Some(rs) = resolved_space {
                            if let Some(arr) = rs.as_array() {
                                if let Some(type_name) = arr.first().and_then(|o| o.as_name()) {
                                    match type_name {
                                        "ICCBased" if arr.len() > 1 => {
                                            if let Ok(dict_obj) = doc.resolve_object(&arr[1]) {
                                                if let Some(dict) = dict_obj.as_dict() {
                                                    let n = dict
                                                        .get("N")
                                                        .and_then(|o| o.as_integer())
                                                        .unwrap_or(3);
                                                    match n {
                                                        1 if !components.is_empty() => {
                                                            let g = components[0];
                                                            gs.fill_color_rgb = (g, g, g);
                                                            handled = true;
                                                        }
                                                        3 if components.len() >= 3 => {
                                                            gs.fill_color_rgb = (
                                                                components[0],
                                                                components[1],
                                                                components[2],
                                                            );
                                                            handled = true;
                                                        }
                                                        4 if components.len() >= 4 => {
                                                            gs.fill_color_rgb = cmyk_to_rgb(
                                                                components[0],
                                                                components[1],
                                                                components[2],
                                                                components[3],
                                                            );
                                                            handled = true;
                                                        }
                                                        _ => {}
                                                    }
                                                }
                                            }
                                        }
                                        "Separation" | "DeviceN" => {
                                            // Inline Separation/DeviceN evaluation used to
                                            // live here as a partial reimplementation of the
                                            // colour-resolver's tint-transform path. Wave 5
                                            // promoted the pipeline to the single source of
                                            // truth — the pipeline runs the full Type 2 / 3 /
                                            // 4 evaluator at paint time and splices the
                                            // resulting RGBA via pipeline_resolve_paint_gs.
                                            // The dispatcher just records the components on
                                            // gs.fill_color_components above; the pipeline
                                            // reads those when the paint op fires. Setting
                                            // gs.fill_color_rgb here would only seed the
                                            // rgba_matches short-circuit, and an inline
                                            // approximation would be wrong for any Type 4 or
                                            // Type 3 tint transform — pin it as "handled"
                                            // (no fallback gray write) and let the pipeline
                                            // own the colour.
                                            handled = true;
                                        }
                                        "Indexed" => {
                                            if !components.is_empty() {
                                                let g = components[0] / 255.0;
                                                gs.fill_color_rgb = (g, g, g);
                                                handled = true;
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }

                        if !handled && !components.is_empty() {
                            let g = components[0];
                            gs.fill_color_rgb = (g, g, g);
                        }
                    }
                }
                // Per ISO 32000-1 §8.6.6.4 / §8.6.6.5: when the fill
                // colour space is /Separation or /DeviceN, record the
                // colorant names + tints for the sidecar's per-paint
                // spot lane mirror. Other spaces clear the slot so a
                // subsequent paint does not inherit stale spot data
                // from a prior /Separation set.
                gs.fill_spot_inks = resolved_space
                    .map(|rs| {
                        crate::rendering::sidecar::extract_paint_spot_inks(rs, components, doc)
                    })
                    .unwrap_or_default();
                log::debug!(
                    "SetFillColor: {} {:?} -> {:?}",
                    space_name,
                    components,
                    gs.fill_color_rgb
                );
            }
            Operator::SetStrokeColor { components } => {
                let gs = gs_stack.current_mut();
                let space_name = gs.stroke_color_space.clone();
                let resolved_space = self.color_spaces.get(&space_name);
                gs.stroke_color_components.clear();
                gs.stroke_color_components.extend_from_slice(components);
                gs.stroke_color_cmyk = None;

                match space_name.as_str() {
                    "DeviceGray" | "G" if !components.is_empty() => {
                        let g = components[0];
                        gs.stroke_color_rgb = (g, g, g);
                    }
                    "DeviceRGB" | "RGB" if components.len() >= 3 => {
                        gs.stroke_color_rgb = (components[0], components[1], components[2]);
                    }
                    "DeviceCMYK" | "CMYK" if components.len() >= 4 => {
                        gs.stroke_color_rgb =
                            cmyk_to_rgb(components[0], components[1], components[2], components[3]);
                        gs.stroke_color_cmyk =
                            Some((components[0], components[1], components[2], components[3]));
                    }
                    _ => {
                        let mut handled = false;
                        if let Some(rs) = resolved_space {
                            if let Some(arr) = rs.as_array() {
                                if let Some(type_name) = arr.first().and_then(|o| o.as_name()) {
                                    match type_name {
                                        "ICCBased" if arr.len() > 1 => {
                                            if let Ok(dict_obj) = doc.resolve_object(&arr[1]) {
                                                if let Some(dict) = dict_obj.as_dict() {
                                                    let n = dict
                                                        .get("N")
                                                        .and_then(|o| o.as_integer())
                                                        .unwrap_or(3);
                                                    match n {
                                                        1 if !components.is_empty() => {
                                                            let g = components[0];
                                                            gs.stroke_color_rgb = (g, g, g);
                                                            handled = true;
                                                        }
                                                        3 if components.len() >= 3 => {
                                                            gs.stroke_color_rgb = (
                                                                components[0],
                                                                components[1],
                                                                components[2],
                                                            );
                                                            handled = true;
                                                        }
                                                        4 if components.len() >= 4 => {
                                                            gs.stroke_color_rgb = cmyk_to_rgb(
                                                                components[0],
                                                                components[1],
                                                                components[2],
                                                                components[3],
                                                            );
                                                            handled = true;
                                                        }
                                                        _ => {}
                                                    }
                                                }
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                        if !handled && !components.is_empty() {
                            let g = components[0];
                            gs.stroke_color_rgb = (g, g, g);
                        }
                    }
                }
                gs.stroke_spot_inks = resolved_space
                    .map(|rs| {
                        crate::rendering::sidecar::extract_paint_spot_inks(rs, components, doc)
                    })
                    .unwrap_or_default();
                log::debug!(
                    "SetStrokeColor: {} {:?} -> {:?}",
                    space_name,
                    components,
                    gs.stroke_color_rgb
                );
            }
            Operator::SetFillColorN { components, name } => {
                let gs = gs_stack.current_mut();
                let space_name = gs.fill_color_space.clone();
                let resolved_space = self.color_spaces.get(&space_name);
                gs.fill_color_components.clear();
                gs.fill_color_components.extend_from_slice(components);
                gs.fill_color_cmyk = None;
                // §8.7.3: retain the pattern name for the Fill path when the
                // active fill space is /Pattern; clear it otherwise so a
                // later device-colour scn cannot paint a stale pattern.
                gs.fill_pattern_name = if space_name == "Pattern" {
                    name.as_ref().map(|n| n.as_str().to_string())
                } else {
                    None
                };

                match space_name.as_str() {
                    "DeviceGray" | "G" if !components.is_empty() => {
                        let g = components[0];
                        gs.fill_color_rgb = (g, g, g);
                    }
                    "DeviceRGB" | "RGB" if components.len() >= 3 => {
                        gs.fill_color_rgb = (components[0], components[1], components[2]);
                    }
                    "DeviceCMYK" | "CMYK" if components.len() >= 4 => {
                        gs.fill_color_rgb =
                            cmyk_to_rgb(components[0], components[1], components[2], components[3]);
                        gs.fill_color_cmyk =
                            Some((components[0], components[1], components[2], components[3]));
                    }
                    _ => {
                        let mut handled = false;
                        if let Some(rs) = resolved_space {
                            if let Some(arr) = rs.as_array() {
                                if let Some(type_name) = arr.first().and_then(|o| o.as_name()) {
                                    match type_name {
                                        "ICCBased" if arr.len() > 1 => {
                                            if let Ok(dict_obj) = doc.resolve_object(&arr[1]) {
                                                if let Some(dict) = dict_obj.as_dict() {
                                                    let n = dict
                                                        .get("N")
                                                        .and_then(|o| o.as_integer())
                                                        .unwrap_or(3);
                                                    match n {
                                                        1 if !components.is_empty() => {
                                                            let g = components[0];
                                                            gs.fill_color_rgb = (g, g, g);
                                                            handled = true;
                                                        }
                                                        3 if components.len() >= 3 => {
                                                            gs.fill_color_rgb = (
                                                                components[0],
                                                                components[1],
                                                                components[2],
                                                            );
                                                            handled = true;
                                                        }
                                                        4 if components.len() >= 4 => {
                                                            gs.fill_color_rgb = cmyk_to_rgb(
                                                                components[0],
                                                                components[1],
                                                                components[2],
                                                                components[3],
                                                            );
                                                            handled = true;
                                                        }
                                                        _ => {}
                                                    }
                                                }
                                            }
                                        }
                                        "Separation" | "DeviceN" => {
                                            // Pipeline owns the colour at paint time —
                                            // see the matching comment in the SetFillColor
                                            // arm above. The dispatcher just records the
                                            // components for the pipeline to read.
                                            //
                                            // BUT: §11.7.4.3 CompatibleOverprint reads
                                            // `gs.fill_color_cmyk` (when populated) /
                                            // `gs.fill_color_rgb` to recover the source
                                            // CMYK for the `B(c_b, c_s)` blend function.
                                            // A DeviceN paint that declares /Process
                                            // attribution (§8.6.6.5) carries process
                                            // colorants directly in its source tints; we
                                            // must populate the graphics-state CMYK
                                            // identity here, otherwise the overprint
                                            // dispatcher reads the stale post-`cs`
                                            // initial `(0,0,0)` RGB and produces a
                                            // constant `(1,1,1,0)` source CMYK
                                            // regardless of actual scn tints.
                                            if type_name == "DeviceN" {
                                                let intent_for_extract =
                                                    crate::color::RenderingIntent::from_pdf_name(
                                                        &gs.rendering_intent,
                                                    );
                                                if let Some(cmyk) =
                                                        crate::rendering::sidecar::extract_process_paint_cmyk(
                                                            rs,
                                                            components,
                                                            doc,
                                                            intent_for_extract,
                                                            Some(&self.icc_transform_cache),
                                                        )
                                                    {
                                                        gs.fill_color_cmyk = Some(cmyk);
                                                        gs.fill_color_rgb = cmyk_to_rgb(
                                                            cmyk.0, cmyk.1, cmyk.2, cmyk.3,
                                                        );
                                                    }
                                            }
                                            handled = true;
                                        }
                                        "Indexed" => {
                                            // Pipeline's resolve_indexed handles index/255
                                            // gray fallback at paint time. The inline path
                                            // used to set gs.fill_color_rgb here to seed
                                            // the rgba_matches short-circuit; the pipeline
                                            // now produces the same value unconditionally,
                                            // so the short-circuit either fires or the
                                            // splice clone runs — either way the colour is
                                            // correct.
                                            handled = true;
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                        if !handled && !components.is_empty() {
                            let g = components[0];
                            gs.fill_color_rgb = (g, g, g);
                        }
                    }
                }
                gs.fill_spot_inks = resolved_space
                    .map(|rs| {
                        crate::rendering::sidecar::extract_paint_spot_inks(rs, components, doc)
                    })
                    .unwrap_or_default();
                log::debug!(
                    "SetFillColorN: {} {:?} -> {:?}",
                    space_name,
                    components,
                    gs.fill_color_rgb
                );
            }
            Operator::SetStrokeColorN { components, .. } => {
                let gs = gs_stack.current_mut();
                let space_name = gs.stroke_color_space.clone();
                let resolved_space = self.color_spaces.get(&space_name);
                gs.stroke_color_components.clear();
                gs.stroke_color_components.extend_from_slice(components);
                gs.stroke_color_cmyk = None;
                match space_name.as_str() {
                    "DeviceGray" | "G" if !components.is_empty() => {
                        let g = components[0];
                        gs.stroke_color_rgb = (g, g, g);
                    }
                    "DeviceRGB" | "RGB" if components.len() >= 3 => {
                        gs.stroke_color_rgb = (components[0], components[1], components[2]);
                    }
                    "DeviceCMYK" | "CMYK" if components.len() >= 4 => {
                        gs.stroke_color_rgb =
                            cmyk_to_rgb(components[0], components[1], components[2], components[3]);
                        gs.stroke_color_cmyk =
                            Some((components[0], components[1], components[2], components[3]));
                    }
                    _ => {
                        let mut handled = false;
                        if let Some(rs) = resolved_space {
                            if let Some(arr) = rs.as_array() {
                                if let Some(type_name) = arr.first().and_then(|o| o.as_name()) {
                                    match type_name {
                                        "ICCBased" if arr.len() > 1 => {
                                            if let Ok(dict_obj) = doc.resolve_object(&arr[1]) {
                                                if let Some(dict) = dict_obj.as_dict() {
                                                    let n = dict
                                                        .get("N")
                                                        .and_then(|o| o.as_integer())
                                                        .unwrap_or(3);
                                                    match n {
                                                        1 if !components.is_empty() => {
                                                            let g = components[0];
                                                            gs.stroke_color_rgb = (g, g, g);
                                                            handled = true;
                                                        }
                                                        3 if components.len() >= 3 => {
                                                            gs.stroke_color_rgb = (
                                                                components[0],
                                                                components[1],
                                                                components[2],
                                                            );
                                                            handled = true;
                                                        }
                                                        4 if components.len() >= 4 => {
                                                            gs.stroke_color_rgb = cmyk_to_rgb(
                                                                components[0],
                                                                components[1],
                                                                components[2],
                                                                components[3],
                                                            );
                                                            handled = true;
                                                        }
                                                        _ => {}
                                                    }
                                                }
                                            }
                                        }
                                        "Separation" | "DeviceN" => {
                                            // Pipeline owns the colour at paint time —
                                            // see the matching comment in the SetFillColor
                                            // arm. The §11.7.4.3 CompatibleOverprint
                                            // source-CMYK reconstruction for /Process-
                                            // attributed DeviceN runs the same way as the
                                            // fill side; see the comment in
                                            // `SetFillColorN` above.
                                            if type_name == "DeviceN" {
                                                let intent_for_extract =
                                                    crate::color::RenderingIntent::from_pdf_name(
                                                        &gs.rendering_intent,
                                                    );
                                                if let Some(cmyk) =
                                                        crate::rendering::sidecar::extract_process_paint_cmyk(
                                                            rs,
                                                            components,
                                                            doc,
                                                            intent_for_extract,
                                                            Some(&self.icc_transform_cache),
                                                        )
                                                    {
                                                        gs.stroke_color_cmyk = Some(cmyk);
                                                        gs.stroke_color_rgb = cmyk_to_rgb(
                                                            cmyk.0, cmyk.1, cmyk.2, cmyk.3,
                                                        );
                                                    }
                                            }
                                            handled = true;
                                        }
                                        "Indexed" => {
                                            // Pipeline's resolve_indexed handles
                                            // index/255 gray fallback at paint time.
                                            handled = true;
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                        if !handled && !components.is_empty() {
                            let g = components[0];
                            gs.stroke_color_rgb = (g, g, g);
                        }
                    }
                }
                gs.stroke_spot_inks = resolved_space
                    .map(|rs| {
                        crate::rendering::sidecar::extract_paint_spot_inks(rs, components, doc)
                    })
                    .unwrap_or_default();
                log::debug!(
                    "SetStrokeColorN: {} {:?} -> {:?}",
                    space_name,
                    components,
                    gs.stroke_color_rgb
                );
            }

            _ => return false,
        }
        true
    }
}
