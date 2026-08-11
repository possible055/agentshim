use super::*;

/// Core multi-ink rendering: allocate one pixmap per referenced ink,
/// walk the content stream once, and extract grayscale data from each.
///
/// ISO 32000-1 §11.7.3 + §11.7.4.2 mandate composite-then-separate
/// when the page declares any transparency or overprint trigger: the
/// §11.4 composite buffer must be evaluated first (process lanes in
/// the page-group blend space, spot lanes as a §11.7.3 sidecar with
/// the §11.7.4.2 BM split), and only after every blend / SMask /
/// knockout has resolved is the result decomposed into per-plate
/// §10.5 output. The detection-gate dispatch at the top of this
/// function picks that path for any page that fires the round-1
/// detection helper. For detection-OFF pages the per-plate walker
/// stays — it is byte-identical to a "no transparency" render at the
/// pixel level and avoids a needless ICC + spot-sidecar allocation.
pub(super) fn render_plates_for_inks(
    doc: &PdfDocument,
    page_num: usize,
    dpi: u32,
    inks: &[String],
    referenced: &[String],
) -> Result<Vec<SeparationPlate>> {
    // Detection-gate dispatch: route detection-ON (transparency-only)
    // pages through the page renderer's composite path so the §11.4
    // transparency model produces the per-plate buffer the per-plate
    // walker (which is SMask-blind, /K-blind, and BM-blind by design)
    // cannot. Pure-overprint pages without any transparency trigger
    // stay on the per-plate walker — its `tint_for_ink` already
    // implements §11.7.4 OP / OPM correctly per-plate, and the
    // composite path's overprint handler is RGB-composite-oriented
    // (its OPM=0 rule additively merges plates, which is wrong for
    // per-plate output where OPM=0 means "replace per-plate"). The
    // gate uses the transparency-only helper for that reason.
    let resources = doc.get_page_resources(page_num)?;
    if crate::rendering::sidecar::page_declares_transparency(doc, &resources) {
        return render_plates_via_composite(doc, page_num, dpi, inks, referenced);
    }

    let (width, height, base_transform) = compute_page_extent(doc, page_num, dpi)?;

    // Partition inks into "needs rendering" vs "short-circuit to empty plate".
    // We track the original index so the output order matches `inks`.
    let mut render_indices: Vec<usize> = Vec::new();
    let mut empty_indices: Vec<usize> = Vec::new();
    for (i, ink) in inks.iter().enumerate() {
        if referenced.iter().any(|r| r == ink) {
            render_indices.push(i);
        } else {
            empty_indices.push(i);
        }
    }

    // Build pixmaps and a parallel `target_inks` slice for the inks we
    // actually need to walk operators for.
    let mut pixmaps: Vec<Pixmap> = Vec::with_capacity(render_indices.len());
    for _ in &render_indices {
        let pixmap = Pixmap::new(width, height)
            .ok_or_else(|| Error::InvalidPdf("Failed to create separation pixmap".to_string()))?;
        pixmaps.push(pixmap);
    }
    let target_inks: Vec<&str> = render_indices.iter().map(|&i| inks[i].as_str()).collect();

    if !pixmaps.is_empty() {
        let color_spaces = load_color_spaces(doc, &resources)?;
        let fonts = load_fonts(doc, &resources);
        let text_rasterizer = TextRasterizer::new();

        let content_data = doc.get_page_content_data(page_num)?;
        let operators = parse_content_stream(&content_data)?;

        let mut ctx = SeparationContext {
            doc,
            text_rasterizer: &text_rasterizer,
            fonts: &fonts,
        };

        execute_separation_operators(
            &mut pixmaps,
            base_transform,
            &operators,
            &mut ctx,
            &resources,
            &color_spaces,
            None,
            &target_inks,
        )?;
    }

    // Re-assemble in original ink order: empty plates for unreferenced
    // inks, extracted R channel for rendered ones.
    let pixel_count = (width as usize) * (height as usize);
    let mut result: Vec<Option<SeparationPlate>> = (0..inks.len()).map(|_| None).collect();

    for (k, &i) in render_indices.iter().enumerate() {
        let mut data = vec![0u8; pixel_count];
        let rgba = pixmaps[k].data();
        for j in 0..pixel_count {
            data[j] = rgba[j * 4];
        }
        result[i] = Some(SeparationPlate {
            ink_name: inks[i].clone(),
            data,
            width,
            height,
        });
    }
    for &i in &empty_indices {
        result[i] = Some(SeparationPlate {
            ink_name: inks[i].clone(),
            data: vec![0u8; pixel_count],
            width,
            height,
        });
    }

    Ok(result
        .into_iter()
        .map(|o| o.expect("plate filled"))
        .collect())
}

/// Composite-then-separate path. Invoked when the page declares any
/// transparency or overprint trigger (round-1 detection helper).
///
/// ISO 32000-1 §11.7.3 — composite the §11.4 transparency model in the
/// process blend space (CMYK), with spot lanes riding alongside per
/// §11.7.3 / §11.7.4.2, then extract per-plate output for the
/// requested ink set per §10.5. Concretely:
///
/// 1. Drive the page renderer's composite path with
///    `force_cmyk_sidecar = true` so the §11.4 buffer survives the
///    render regardless of `OutputIntent` presence.
/// 2. Harvest the populated sidecar
///    ([`PageRenderer::take_cmyk_sidecar`]).
/// 3. For each requested ink: if it matches a process colorant
///    ("Cyan", "Magenta", "Yellow", "Black") read the CMYK channel
///    from `process_plate`; otherwise look up the spot plane from
///    `spot_plate`. Inks neither named in the sidecar's spot set nor
///    matching a process colorant produce an all-zero plate (the
///    §8.6.6.3 "no plate" semantic).
///
/// The `referenced` set is honoured for the same short-circuit the
/// per-plate path uses — an ink that is not referenced on the page
/// produces an all-zero plate without consulting the sidecar.
pub(super) fn render_plates_via_composite(
    doc: &PdfDocument,
    page_num: usize,
    dpi: u32,
    inks: &[String],
    referenced: &[String],
) -> Result<Vec<SeparationPlate>> {
    use crate::rendering::page_renderer::{PageRenderer, RenderOptions};

    let mut renderer = PageRenderer::new(RenderOptions::with_dpi(dpi).as_raw());
    renderer.force_cmyk_sidecar = true;
    let rendered = renderer.render_page(doc, page_num)?;
    let width = rendered.width;
    let height = rendered.height;
    let pixel_count = (width as usize) * (height as usize);

    // The composite path may decline to allocate a sidecar if the
    // detection trigger flickers off between the separation entry
    // point and the renderer (e.g. a page whose resources hash
    // resolved without ExtGState). Fall back to an all-zero stack —
    // this matches the per-plate walker's behaviour on a page that
    // declares no inks.
    let sidecar = renderer.take_cmyk_sidecar();
    let mut plates: Vec<SeparationPlate> = Vec::with_capacity(inks.len());
    for ink in inks {
        let mut data = vec![0u8; pixel_count];
        // §8.6.6.3 "no plate" branch — unreferenced inks short-circuit.
        if !referenced.iter().any(|r| r == ink) {
            plates.push(SeparationPlate {
                ink_name: ink.clone(),
                data,
                width,
                height,
            });
            continue;
        }
        if let Some(s) = sidecar.as_ref() {
            if matches!(ink.as_str(), "Cyan" | "Magenta" | "Yellow" | "Black") {
                if let Some(plate) = s.process_plate(ink) {
                    data = plate;
                }
            } else if let Some(lane) = s.spot_plate(ink) {
                data = lane.to_vec();
            }
        }
        plates.push(SeparationPlate {
            ink_name: ink.clone(),
            data,
            width,
            height,
        });
    }
    Ok(plates)
}

/// Page extent computation (width/height in pixels and the base
/// transform that maps PDF user space into the pixmap).
pub(super) fn compute_page_extent(
    doc: &PdfDocument,
    page_num: usize,
    dpi: u32,
) -> Result<(u32, u32, Transform)> {
    let page_info = doc.get_page_info(page_num)?;
    let media_box = page_info.media_box;

    // `%` is a remainder and preserves sign, so a legal negative /Rotate (e.g. -90,
    // equivalent to 270 per ISO 32000-1 s7.7.3.3 Table 30) matched neither 90 nor
    // 270 below and the page rendered unrotated. rem_euclid normalizes to 0..359,
    // matching get_page_rotation's own `((raw % 360) + 360) % 360` convention.
    let rotation = page_info.rotation.rem_euclid(360);
    let (page_w, page_h) = if rotation == 90 || rotation == 270 {
        (media_box.height, media_box.width)
    } else {
        (media_box.width, media_box.height)
    };
    let scale = dpi as f32 / 72.0;
    let width = (page_w * scale).ceil() as u32;
    let height = (page_h * scale).ceil() as u32;

    let base_transform = match rotation {
        90 => Transform::from_translate(-media_box.x, -media_box.y)
            .post_concat(Transform::from_row(0.0, scale, scale, 0.0, 0.0, 0.0)),
        180 => Transform::from_translate(-media_box.x, -media_box.y)
            .post_scale(-scale, scale)
            .post_translate(media_box.width * scale, 0.0),
        270 => Transform::from_translate(-media_box.x, -media_box.y).post_concat(
            Transform::from_row(0.0, scale, -scale, 0.0, media_box.height * scale, 0.0),
        ),
        _ => Transform::from_translate(-media_box.x, -media_box.y)
            .post_scale(scale, -scale)
            .post_translate(0.0, page_h * scale),
    };

    Ok((width, height, base_transform))
}
