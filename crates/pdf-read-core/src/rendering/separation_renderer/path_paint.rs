use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) fn paint_path_to_plates(
    path: &tiny_skia::Path,
    fill_rule: Option<FillRule>,
    stroke: bool,
    pixmaps: &mut [Pixmap],
    target_inks: &[&str],
    target_inks_owned: &[InkName],
    base_transform: Transform,
    graphics_state: &GraphicsState,
    color_state: Option<&SeparationColorState>,
    color_spaces: &HashMap<String, Object>,
    resources: &Object,
    document: &PdfDocument,
    clip: Option<&Mask>,
    pipeline: &ResolutionPipeline,
    backend: &mut SeparationBackend,
) -> Result<()> {
    let empty = SeparationColorState::new();
    let color_state = color_state.unwrap_or(&empty);
    let transform = combine_transforms(base_transform, &graphics_state.ctm);

    if let Some(fill_rule) = fill_rule {
        if side_uses_pipeline(true, graphics_state, color_spaces, resources, document) {
            paint_through_pipeline(
                true,
                Some(fill_rule),
                path,
                pixmaps,
                target_inks_owned,
                base_transform,
                graphics_state,
                color_state,
                color_spaces,
                resources,
                document,
                clip,
                pipeline,
                backend,
            )?;
        } else {
            for (index, &ink) in target_inks.iter().enumerate() {
                if let PaintAction::Paint(tint) = tint_for_ink(
                    true,
                    graphics_state,
                    color_spaces,
                    resources,
                    document,
                    ink,
                    &color_state.fill_components,
                    &color_state.stroke_components,
                ) {
                    fill_separation(&mut pixmaps[index], path, transform, tint, fill_rule, clip);
                }
            }
        }
    }

    if stroke {
        if side_uses_pipeline(false, graphics_state, color_spaces, resources, document) {
            paint_through_pipeline(
                false,
                None,
                path,
                pixmaps,
                target_inks_owned,
                base_transform,
                graphics_state,
                color_state,
                color_spaces,
                resources,
                document,
                clip,
                pipeline,
                backend,
            )?;
        } else {
            for (index, &ink) in target_inks.iter().enumerate() {
                if let PaintAction::Paint(tint) = tint_for_ink(
                    false,
                    graphics_state,
                    color_spaces,
                    resources,
                    document,
                    ink,
                    &color_state.fill_components,
                    &color_state.stroke_components,
                ) {
                    stroke_separation(
                        &mut pixmaps[index],
                        path,
                        transform,
                        graphics_state,
                        tint,
                        clip,
                    );
                }
            }
        }
    }

    Ok(())
}
