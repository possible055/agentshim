use super::*;

/// Stroke a path into the separation pixmap with the given tint value.
pub(super) fn stroke_separation(
    pixmap: &mut Pixmap,
    path: &tiny_skia::Path,
    transform: Transform,
    gs: &GraphicsState,
    tint: f32,
    clip: Option<&Mask>,
) {
    let gray = (tint.clamp(0.0, 1.0) * 255.0).round() as u8;
    let color = tiny_skia::Color::from_rgba8(gray, gray, gray, 255);
    let mut paint = tiny_skia::Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;

    let mut stroke = tiny_skia::Stroke::default();
    stroke.width = gs.line_width;
    stroke.line_cap = match gs.line_cap {
        1 => tiny_skia::LineCap::Round,
        2 => tiny_skia::LineCap::Square,
        _ => tiny_skia::LineCap::Butt,
    };
    stroke.line_join = match gs.line_join {
        1 => tiny_skia::LineJoin::Round,
        2 => tiny_skia::LineJoin::Bevel,
        _ => tiny_skia::LineJoin::Miter,
    };
    stroke.miter_limit = gs.miter_limit;

    if !gs.dash_pattern.0.is_empty() {
        stroke.dash = tiny_skia::StrokeDash::new(gs.dash_pattern.0.clone(), gs.dash_pattern.1);
    }

    pixmap.stroke_path(path, &paint, &stroke, transform, clip);
}

/// Apply a pending clip path to the clip stack.
///
/// The clip mask is identical across all plates — it depends only on the
/// path, fill rule, current transform, and pixmap dimensions (which are
/// shared). So we build it once and store it on the shared clip stack.
pub(super) fn apply_separation_clip(
    pending: &mut Option<(tiny_skia::Path, FillRule)>,
    clip_stack: &mut Vec<Option<Mask>>,
    pixmap_width: u32,
    pixmap_height: u32,
    base_transform: Transform,
    gs_stack: &GraphicsStateStack,
) {
    if let Some((path, fill_rule)) = pending.take() {
        // No pixmaps means no plates to clip — bail out early. Width/height
        // would be zero and Mask::new would refuse them anyway.
        if pixmap_width == 0 || pixmap_height == 0 {
            return;
        }
        let gs = gs_stack.current();
        let transform = combine_transforms(base_transform, &gs.ctm);

        if let Some(path_transformed) = path.transform(transform) {
            let mut new_mask = Mask::new(pixmap_width, pixmap_height).unwrap();
            new_mask.fill_path(&path_transformed, fill_rule, true, Transform::identity());

            if let Some(Some(current_mask)) = clip_stack.last() {
                let mut combined = current_mask.clone();
                let combined_data = combined.data_mut();
                let new_data = new_mask.data();
                for i in 0..combined_data.len() {
                    combined_data[i] = ((combined_data[i] as u32 * new_data[i] as u32) / 255) as u8;
                }
                *clip_stack.last_mut().unwrap() = Some(combined);
            } else {
                *clip_stack.last_mut().unwrap() = Some(new_mask);
            }
        }
    }
}

/// Parse a form XObject matrix from its dictionary.
pub(super) fn parse_form_matrix(dict: &HashMap<String, Object>) -> Transform {
    if let Some(Object::Array(arr)) = dict.get("Matrix") {
        let get_f32 = |i: usize| -> f32 {
            match arr.get(i) {
                Some(Object::Real(v)) => *v as f32,
                Some(Object::Integer(v)) => *v as f32,
                _ => {
                    if i == 0 || i == 3 {
                        1.0
                    } else {
                        0.0
                    }
                }
            }
        };
        Transform::from_row(
            get_f32(0),
            get_f32(1),
            get_f32(2),
            get_f32(3),
            get_f32(4),
            get_f32(5),
        )
    } else {
        Transform::identity()
    }
}

/// Combine two transformations (base + CTM).
pub(super) fn combine_transforms(base: Transform, ctm: &Matrix) -> Transform {
    base.pre_concat(Transform::from_row(
        ctm.a, ctm.b, ctm.c, ctm.d, ctm.e, ctm.f,
    ))
}
