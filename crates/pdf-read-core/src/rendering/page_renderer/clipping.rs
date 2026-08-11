use super::*;

pub(super) fn apply_pending_clip(
    pending_clip: &mut Option<(tiny_skia::Path, tiny_skia::FillRule)>,
    clip_stack: &mut Vec<Option<tiny_skia::Mask>>,
    pixmap: &Pixmap,
    base_transform: Transform,
    gs_stack: &GraphicsStateStack,
) {
    if let Some((path, fill_rule)) = pending_clip.take() {
        #[cfg(test)]
        APC_MATERIALIZED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let gs = gs_stack.current();
        let transform = combine_transforms(base_transform, &gs.ctm);

        let Some(slot) = clip_stack.last_mut() else {
            return;
        };
        match slot {
            // Intersect the new clip path into the current scope's mask in
            // place. tiny_skia::Mask::intersect_path allocates one submask,
            // rasterizes the path into it, then folds it into `self` via the
            // library's rounded `(a*b)/255` premultiply — replacing the
            // previous code path which additionally cloned the current mask
            // (a full page-sized memcpy) before running an equivalent scalar
            // multiply loop. The clone was redundant: every `q` already pushes
            // a cloned mask onto `clip_stack`, so the top-of-stack mask at the
            // current depth is already this scope's private copy and may be
            // mutated in place.
            Some(existing_mask) => {
                existing_mask.intersect_path(&path, fill_rule, true, transform);
            }
            None => {
                let mut new_mask = tiny_skia::Mask::new(pixmap.width(), pixmap.height()).unwrap();
                new_mask.fill_path(&path, fill_rule, true, transform);
                *slot = Some(new_mask);
            }
        }
    }
}

/// Build a `tiny_skia::Mask` that clips an axial shading to the
/// gradient slab defined by `/Extend`. Returns `None` for the
/// `[true true]` case (no clipping needed beyond the inherited
/// `clip_mask`, which the caller handles directly).
///
/// The slab is the strip between the two lines perpendicular to the
/// axis through `p0` and `p1`. Asymmetric extends paint the strip
/// plus one half-plane past the extended end. The returned mask is
/// the intersection of the slab with the inherited `clip_mask`.
pub(super) fn build_axial_extend_clip(
    pixmap: &Pixmap,
    p0: tiny_skia::Point,
    p1: tiny_skia::Point,
    extend_start: bool,
    extend_end: bool,
    inherited: Option<&tiny_skia::Mask>,
) -> Option<tiny_skia::Mask> {
    if extend_start && extend_end {
        return None;
    }

    let w = pixmap.width() as f32;
    let h = pixmap.height() as f32;

    // Axis vector (device-space) and unit-normal perpendicular. A
    // degenerate axis (p0 ≈ p1) collapses to a zero-area gradient; no
    // valid slab can be constructed, so skip the extra clip and let
    // the inherited mask carry through.
    let dx = p1.x - p0.x;
    let dy = p1.y - p0.y;
    let len = (dx * dx + dy * dy).sqrt();
    if !len.is_finite() || len < 1.0e-6 {
        return None;
    }
    let ux = dx / len;
    let uy = dy / len;
    // Perpendicular unit vector (rotated +90°).
    let px = -uy;
    let py = ux;

    // Far perpendicular extent — large enough to cover the pixmap
    // diagonal from any axis position. Using 4× the diagonal stays
    // robust against off-page axis endpoints.
    let diag = (w * w + h * h).sqrt();
    let far_perp = 4.0 * diag;

    // The "axis-direction" extent must reach past the pixmap from
    // either endpoint when /Extend on that side is true. Same 4×
    // diagonal margin keeps the test robust.
    let far_axis_start = if extend_start { 4.0 * diag } else { 0.0 };
    let far_axis_end = if extend_end { 4.0 * diag } else { 0.0 };

    // Four corners of the slab polygon, walking
    // (start_minus_perp, start_plus_perp, end_plus_perp, end_minus_perp)
    // so the polygon is convex / non-self-intersecting.
    let start_x = p0.x - far_axis_start * ux;
    let start_y = p0.y - far_axis_start * uy;
    let end_x = p1.x + far_axis_end * ux;
    let end_y = p1.y + far_axis_end * uy;
    let mut pb = PathBuilder::new();
    pb.move_to(start_x - far_perp * px, start_y - far_perp * py);
    pb.line_to(start_x + far_perp * px, start_y + far_perp * py);
    pb.line_to(end_x + far_perp * px, end_y + far_perp * py);
    pb.line_to(end_x - far_perp * px, end_y - far_perp * py);
    pb.close();
    let path = pb.finish()?;

    let mut mask = tiny_skia::Mask::new(pixmap.width(), pixmap.height())?;
    mask.fill_path(
        &path,
        tiny_skia::FillRule::Winding,
        true,
        Transform::identity(),
    );
    Some(intersect_with_inherited(mask, inherited))
}

/// Build a `tiny_skia::Mask` that clips a radial shading to the
/// gradient region defined by `/Extend`. Returns `None` for the
/// `[true true]` case.
///
/// Strategy for the common `r0 < r1` case:
/// * `Extend[1] = false` → exclude pixels outside the outer circle.
/// * `Extend[0] = false` → exclude pixels inside the inner circle
///   (forms an annulus when combined with the outer exclusion).
pub(super) fn build_radial_extend_clip(
    pixmap: &Pixmap,
    start: (tiny_skia::Point, f32),
    end: (tiny_skia::Point, f32),
    extend_start: bool,
    extend_end: bool,
    inherited: Option<&tiny_skia::Mask>,
) -> Option<tiny_skia::Mask> {
    if extend_start && extend_end {
        return None;
    }

    let (c0, r0) = start;
    let (c1, r1) = end;

    // For non-concentric circles the spec's family-of-circles cone
    // shape is more complex than a simple annulus; the best-effort
    // approximation here is the union of the disks at each end. This
    // captures the common "spotlight" pattern (small inner point,
    // large outer circle) without painting outside the outer circle.
    //
    // When `Extend[0] = false` we also exclude the inner disk
    // (subtract it via an even-odd fill rule).
    let mut mask = tiny_skia::Mask::new(pixmap.width(), pixmap.height())?;

    let outer_path = {
        let mut pb = PathBuilder::new();
        if !extend_end {
            // Outer boundary is the outer circle plus the inner
            // circle padded outward (for the inner-padded extend-true
            // case we just use the outer circle).
            pb.push_circle(c1.x, c1.y, r1.max(1.0e-3));
        } else {
            // No outer-side clip: the outer boundary is the full
            // pixmap rectangle.
            let rect = tiny_skia::Rect::from_xywh(
                0.0,
                0.0,
                pixmap.width() as f32,
                pixmap.height() as f32,
            )?;
            pb.push_rect(rect);
        }
        pb.finish()?
    };
    mask.fill_path(
        &outer_path,
        tiny_skia::FillRule::Winding,
        true,
        Transform::identity(),
    );

    if !extend_start && r0 > 1.0e-3 {
        // Subtract the inner disk by painting black into the mask.
        // tiny-skia's `Mask` is a single-channel u8 buffer; "subtract"
        // by filling the inner path into a fresh inner-mask and then
        // multiplying mask by (1 - inner_mask).
        let mut inner_mask = tiny_skia::Mask::new(pixmap.width(), pixmap.height())?;
        let mut pb = PathBuilder::new();
        pb.push_circle(c0.x, c0.y, r0);
        if let Some(inner_path) = pb.finish() {
            inner_mask.fill_path(
                &inner_path,
                tiny_skia::FillRule::Winding,
                true,
                Transform::identity(),
            );
            let outer_data = mask.data_mut();
            let inner_data = inner_mask.data();
            for i in 0..outer_data.len() {
                let outside_inner = 255u32 - inner_data[i] as u32;
                outer_data[i] = ((outer_data[i] as u32 * outside_inner) / 255) as u8;
            }
        }
    }

    Some(intersect_with_inherited(mask, inherited))
}

/// Multiply the per-pixel coverage of `mask` by the inherited
/// `clip_mask` so the gradient is bounded by both at once.
pub(super) fn intersect_with_inherited(
    mut mask: tiny_skia::Mask,
    inherited: Option<&tiny_skia::Mask>,
) -> tiny_skia::Mask {
    if let Some(existing) = inherited {
        let data = mask.data_mut();
        let other = existing.data();
        // Both masks are sized to the pixmap, so the buffers match.
        let n = data.len().min(other.len());
        for i in 0..n {
            data[i] = ((data[i] as u32 * other[i] as u32) / 255) as u8;
        }
    }
    mask
}
