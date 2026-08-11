use super::*;

/// Collect all ink names present on a page.
///
/// CMYK is always returned regardless of whether the page actually uses
/// CMYK content; unused process plates are filtered out by the per-plate
/// short-circuit in [`render_separations`].
///
/// Spot inks come from [`PdfDocument::get_page_inks_deep`], which walks
/// the page's content stream into nested Form XObjects (§8.10) so spots
/// declared in form-local resources are discovered.
pub(super) fn collect_page_inks(doc: &PdfDocument, page_num: usize) -> Result<Vec<String>> {
    let mut inks = vec![
        "Cyan".to_string(),
        "Magenta".to_string(),
        "Yellow".to_string(),
        "Black".to_string(),
    ];

    let spot_inks = doc.get_page_inks_deep(page_num)?;
    for ink in spot_inks {
        if !inks.contains(&ink) {
            inks.push(ink);
        }
    }

    Ok(inks)
}

/// Walk the content stream (and any Form XObjects it references) and
/// collect every ink name that could possibly appear on the page.
pub(super) fn collect_referenced_inks(doc: &PdfDocument, page_num: usize) -> Result<Vec<String>> {
    let resources = doc.get_page_resources(page_num)?;
    let color_spaces = load_color_spaces(doc, &resources)?;
    let content_data = doc.get_page_content_data(page_num)?;
    let operators = parse_content_stream(&content_data)?;
    let mut referenced: Vec<String> = Vec::new();
    let mut visited: Vec<String> = Vec::new();
    scan_operators_for_inks(
        &operators,
        doc,
        &resources,
        &color_spaces,
        &mut referenced,
        &mut visited,
    )?;
    Ok(referenced)
}

pub(super) fn scan_operators_for_inks(
    operators: &[Operator],
    doc: &PdfDocument,
    resources: &Object,
    color_spaces: &HashMap<String, Object>,
    referenced: &mut Vec<String>,
    visited: &mut Vec<String>,
) -> Result<()> {
    let xobjects = match resources {
        Object::Dictionary(rd) => rd.get("XObject").and_then(|o| doc.resolve_object(o).ok()),
        _ => None,
    };

    let push = |list: &mut Vec<String>, name: &str| {
        if !list.iter().any(|s| s == name) {
            list.push(name.to_string());
        }
    };

    for op in operators {
        match op {
            Operator::SetFillCmyk { .. } | Operator::SetStrokeCmyk { .. } => {
                push(referenced, "Cyan");
                push(referenced, "Magenta");
                push(referenced, "Yellow");
                push(referenced, "Black");
            }
            Operator::SetFillColorSpace { name } | Operator::SetStrokeColorSpace { name } => {
                inks_from_space(name, color_spaces, resources, doc, referenced);
            }
            Operator::Do { name } => {
                if visited.iter().any(|s| s == name) {
                    continue;
                }
                visited.push(name.clone());
                if let Some(xobj_dict) = xobjects.as_ref().and_then(|o| o.as_dict()) {
                    if let Some(xobj_ref_obj) = xobj_dict.get(name) {
                        if let Ok(xobj) = doc.resolve_object(xobj_ref_obj) {
                            if let Object::Stream { ref dict, .. } = xobj {
                                let subtype = dict.get("Subtype").and_then(|o| o.as_name());
                                if subtype == Some("Form") {
                                    let stream_data = if let Some(r) = xobj_ref_obj.as_reference() {
                                        doc.decode_stream_with_encryption(&xobj, r)?
                                    } else {
                                        xobj.decode_stream_data()?
                                    };
                                    let form_resources = if let Some(res) = dict.get("Resources") {
                                        doc.resolve_object(res)?
                                    } else {
                                        resources.clone()
                                    };
                                    let form_cs = load_color_spaces(doc, &form_resources)?;
                                    let mut merged_cs = color_spaces.clone();
                                    merged_cs.extend(form_cs);
                                    if let Ok(form_ops) = parse_content_stream(&stream_data) {
                                        scan_operators_for_inks(
                                            &form_ops,
                                            doc,
                                            &form_resources,
                                            &merged_cs,
                                            referenced,
                                            visited,
                                        )?;
                                    }
                                } else if subtype == Some("Image") {
                                    // §8.9: image XObjects carry their own
                                    // /ColorSpace declaration and contribute
                                    // their colorants without needing a
                                    // colour-setting operator in the content
                                    // stream. Surface those inks so the
                                    // per-plate short-circuit doesn't drop
                                    // the image's plates as empty.
                                    let resolved = resolve_image_color_space(
                                        dict,
                                        color_spaces,
                                        resources,
                                        doc,
                                    );
                                    match resolved {
                                        ResolvedSpace::Cmyk | ResolvedSpace::IccCmyk => {
                                            push(referenced, "Cyan");
                                            push(referenced, "Magenta");
                                            push(referenced, "Yellow");
                                            push(referenced, "Black");
                                        }
                                        ResolvedSpace::Separation(ink) => {
                                            if ink != "None" && !ink.is_empty() {
                                                if ink == "All" {
                                                    push(referenced, "Cyan");
                                                    push(referenced, "Magenta");
                                                    push(referenced, "Yellow");
                                                    push(referenced, "Black");
                                                } else {
                                                    push(referenced, &ink);
                                                }
                                            }
                                        }
                                        ResolvedSpace::DeviceN(names) => {
                                            for n in names {
                                                if n != "None" && !n.is_empty() {
                                                    if n == "All" {
                                                        push(referenced, "Cyan");
                                                        push(referenced, "Magenta");
                                                        push(referenced, "Yellow");
                                                        push(referenced, "Black");
                                                    } else {
                                                        push(referenced, &n);
                                                    }
                                                }
                                            }
                                        }
                                        // RGB / Gray / Unknown contribute no
                                        // plates per the renderer's policy.
                                        _ => {}
                                    }
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

pub(super) fn inks_from_space(
    space_name: &str,
    color_spaces: &HashMap<String, Object>,
    resources: &Object,
    doc: &PdfDocument,
    out: &mut Vec<String>,
) {
    // Honour DefaultCMYK/RGB/Gray remap (RED #2 — see resolve_color_space).
    let space = resolve_color_space(space_name, color_spaces, resources, doc);
    match space {
        ResolvedSpace::Cmyk | ResolvedSpace::IccCmyk => {
            for ink in ["Cyan", "Magenta", "Yellow", "Black"] {
                if !out.iter().any(|s| s == ink) {
                    out.push(ink.to_string());
                }
            }
        }
        ResolvedSpace::Separation(name) => {
            // §8.6.6.4: /All marks every output separation — list CMYK so the
            // per-plate short-circuit in render_separations doesn't skip them.
            // /None paints nothing and never names a plate.
            if name == "All" {
                for ink in ["Cyan", "Magenta", "Yellow", "Black"] {
                    if !out.iter().any(|s| s == ink) {
                        out.push(ink.to_string());
                    }
                }
            } else if name != "None" && !out.iter().any(|s| s == &name) {
                out.push(name);
            }
        }
        ResolvedSpace::DeviceN(names) => {
            for n in names {
                if n == "All" {
                    for ink in ["Cyan", "Magenta", "Yellow", "Black"] {
                        if !out.iter().any(|s| s == ink) {
                            out.push(ink.to_string());
                        }
                    }
                } else if n != "None" && !out.iter().any(|s| s == &n) {
                    out.push(n);
                }
            }
        }
        ResolvedSpace::Rgb
        | ResolvedSpace::Gray
        | ResolvedSpace::IccRgb
        | ResolvedSpace::IccGray
        | ResolvedSpace::Unknown => {}
    }
}
