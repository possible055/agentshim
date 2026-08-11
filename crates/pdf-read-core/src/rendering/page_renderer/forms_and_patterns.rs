use super::*;

impl PageRenderer {
    /// Render a Form XObject by parsing its content stream recursively.
    ///
    /// Per PDF spec §8.10, a Form XObject contains its own content stream,
    /// optional /Matrix transform, and optional /Resources dictionary.
    pub(super) fn render_form_xobject(
        &mut self,
        pixmap: &mut Pixmap,
        dict: &std::collections::HashMap<String, Object>,
        data: &[u8],
        parent_transform: Transform,
        doc: &PdfDocument,
        page_num: usize,
        parent_resources: &Object,
    ) -> Result<()> {
        // Parse /Matrix from form dict (default: identity)
        let form_matrix = if let Some(Object::Array(arr)) = dict.get("Matrix") {
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
        };

        // Combine parent transform with form matrix
        let combined_transform = parent_transform.pre_concat(form_matrix);

        // Check for transparency group (PDF spec section 11.6.6)
        let is_transparency_group = dict
            .get("Group")
            .and_then(|g| g.as_dict())
            .map(|gd| gd.get("S").and_then(|s| s.as_name()) == Some("Transparency"))
            .unwrap_or(false);

        // Get form's /Resources (or fall back to parent resources)
        let form_resources = if let Some(res) = dict.get("Resources") {
            doc.resolve_object(res)?
        } else {
            parent_resources.clone()
        };

        // Parse form content stream
        let operators = match parse_content_stream(data) {
            Ok(ops) => ops,
            Err(e) => {
                return Err(e);
            }
        };

        if is_transparency_group {
            // Per PDF spec 11.6.6: Render transparency group to a separate pixmap,
            // then composite onto the parent. For isolated groups (I=true), the
            // initial backdrop is fully transparent.
            let is_isolated = dict
                .get("Group")
                .and_then(|g| g.as_dict())
                .and_then(|gd| gd.get("I"))
                .map(|i| match i {
                    Object::Boolean(b) => *b,
                    _ => false,
                })
                .unwrap_or(false);

            // ISO 32000-1:2008 §11.4.6.2 — knockout flag. A knockout group
            // composites each element against the group's initial backdrop
            // rather than against the accumulated paint from earlier
            // elements. Later elements override earlier ones in regions
            // where both contribute.
            let is_knockout = dict
                .get("Group")
                .and_then(|g| g.as_dict())
                .and_then(|gd| gd.get("K"))
                .map(|k| match k {
                    Object::Boolean(b) => *b,
                    _ => false,
                })
                .unwrap_or(false);

            log::debug!(
                "Rendering transparency group (isolated={}, knockout={})",
                is_isolated,
                is_knockout
            );

            // Create a separate pixmap for the group
            let mut group_pixmap =
                Pixmap::new(pixmap.width(), pixmap.height()).ok_or_else(|| {
                    crate::error::Error::InvalidPdf("Failed to create group pixmap".into())
                })?;

            if !is_isolated {
                // Non-isolated: copy parent content as initial backdrop
                group_pixmap.data_mut().copy_from_slice(pixmap.data());
            }
            // Isolated groups start fully transparent (default Pixmap state)

            if is_knockout {
                // §11.4.6.2: snapshot the initial backdrop, then composite
                // each element separately against it. The accumulator
                // starts as the backdrop; each paint operator's result is
                // merged in so later paints override earlier ones in
                // overlap regions.
                self.execute_knockout_group(
                    &mut group_pixmap,
                    combined_transform,
                    &operators,
                    doc,
                    page_num,
                    &form_resources,
                )?;
            } else {
                // Execute operators into the group pixmap
                self.execute_operators(
                    &mut group_pixmap,
                    combined_transform,
                    &operators,
                    doc,
                    page_num,
                    &form_resources,
                )?;
            }

            if is_isolated {
                // Composite the isolated group onto the parent using over blending
                pixmap.draw_pixmap(
                    0,
                    0,
                    group_pixmap.as_ref(),
                    &tiny_skia::PixmapPaint::default(),
                    Transform::identity(),
                    None,
                );
            } else {
                // Non-isolated: the group pixmap IS the result (it started with parent content)
                pixmap.data_mut().copy_from_slice(group_pixmap.data());
            }
        } else {
            // Non-group form XObject: render directly
            self.execute_operators(
                pixmap,
                combined_transform,
                &operators,
                doc,
                page_num,
                &form_resources,
            )?;
        }

        Ok(())
    }

    /// Rasterise a `/PatternType 1` tiling pattern into the current fill
    /// region (ISO 32000-1:2008 §8.7.3).
    ///
    /// A tiling pattern paints a small cell — its own content stream
    /// clipped to `/BBox` — repeated on a lattice spaced by `/XStep` ×
    /// `/YStep` in pattern space. The pattern `/Matrix` maps pattern
    /// space to the default (initial) coordinate system of the pattern's
    /// parent content stream, here taken as `base_transform` (the device
    /// transform in effect before the current CTM), NOT the CTM active at
    /// fill time.
    ///
    /// `/PaintType 1` (coloured) cells supply their own colour; `/PaintType 2`
    /// (uncoloured) cells are painted in the current fill colour
    /// (`gs.fill_color_rgb`).
    ///
    /// Returns `Ok(true)` when the region was painted — either tiled, or
    /// (on a perf/geometry guard) flooded with the cell's average colour —
    /// and `Ok(false)` when the referenced pattern is not a usable tiling
    /// pattern (`/PatternType 2` shading, missing/malformed dict, or an
    /// over-large cell), so the caller paints its normal solid fill.
    /// Never panics and never loops unboundedly.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn fill_with_tiling_pattern(
        &mut self,
        pixmap: &mut Pixmap,
        path: &tiny_skia::Path,
        base_transform: Transform,
        path_transform: Transform,
        fill_rule: tiny_skia::FillRule,
        clip: Option<&tiny_skia::Mask>,
        gs: &GraphicsState,
        doc: &PdfDocument,
        page_num: usize,
        resources: &Object,
    ) -> Result<bool> {
        // Cap the offscreen cell raster and the tile count so a pathological
        // pattern cannot exhaust memory or spin. Beyond these limits we fall
        // back to a solid flood (average colour) or defer to the caller.
        const MAX_CELL_PX: u32 = 4096;
        const MAX_TILES: i64 = 1_000_000;

        let Some(pattern_name) = gs.fill_pattern_name.as_deref() else {
            return Ok(false);
        };

        // Resources/Pattern/<name> -> pattern object (a stream for tiling).
        let Some(res_dict) = resources.as_dict() else {
            return Ok(false);
        };
        let Some(pattern_group) = res_dict.get("Pattern") else {
            return Ok(false);
        };
        let pattern_group = doc.resolve_object(pattern_group)?;
        let Some(pattern_map) = pattern_group.as_dict() else {
            return Ok(false);
        };
        let Some(pattern_entry) = pattern_map.get(pattern_name) else {
            return Ok(false);
        };
        let pattern_ref = pattern_entry.as_reference();
        let pattern_obj = doc.resolve_object(pattern_entry)?;
        let Some(pdict) = pattern_obj.as_dict() else {
            return Ok(false);
        };

        // Only tiling patterns (PatternType 1) are handled here; shading
        // patterns (PatternType 2) are left to the caller's solid fallback.
        if pdict
            .get("PatternType")
            .and_then(|o| o.as_integer())
            .unwrap_or(1)
            != 1
        {
            return Ok(false);
        }
        let paint_type = pdict
            .get("PaintType")
            .and_then(|o| o.as_integer())
            .unwrap_or(1);

        let num = |o: &Object| -> Option<f32> {
            o.as_integer()
                .map(|i| i as f32)
                .or_else(|| o.as_real().map(|r| r as f32))
        };
        let read_array = |key: &str, n: usize| -> Option<Vec<f32>> {
            let arr = pdict.get(key)?.as_array()?;
            if arr.len() < n {
                return None;
            }
            arr.iter().take(n).map(&num).collect()
        };

        let Some(bbox) = read_array("BBox", 4) else {
            return Ok(false);
        };
        let x_step = pdict
            .get("XStep")
            .and_then(&num)
            .unwrap_or(bbox[2] - bbox[0]);
        let y_step = pdict
            .get("YStep")
            .and_then(&num)
            .unwrap_or(bbox[3] - bbox[1]);
        let m = read_array("Matrix", 6).unwrap_or_else(|| vec![1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);
        let pattern_matrix = Transform::from_row(m[0], m[1], m[2], m[3], m[4], m[5]);

        // Own the pattern's resource dict before releasing the borrow on
        // `pattern_obj` (needed again to decode the stream).
        let pattern_resources = match pdict.get("Resources") {
            Some(r) => doc.resolve_object(r)?,
            None => resources.clone(),
        };

        // Pattern space -> device.
        let t = base_transform.pre_concat(pattern_matrix);
        let map = |x: f32, y: f32| -> (f32, f32) {
            (x * t.sx + y * t.kx + t.tx, x * t.ky + y * t.sy + t.ty)
        };

        // Device bounding box of the /BBox cell rectangle.
        let corners = [
            map(bbox[0], bbox[1]),
            map(bbox[2], bbox[1]),
            map(bbox[2], bbox[3]),
            map(bbox[0], bbox[3]),
        ];
        let (mut cminx, mut cminy, mut cmaxx, mut cmaxy) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for (x, y) in corners {
            cminx = cminx.min(x);
            cminy = cminy.min(y);
            cmaxx = cmaxx.max(x);
            cmaxy = cmaxy.max(y);
        }
        if ![cminx, cminy, cmaxx, cmaxy].iter().all(|v| v.is_finite()) {
            return Ok(false);
        }
        let cw = (cmaxx - cminx).ceil();
        let ch = (cmaxy - cminy).ceil();
        if !(1.0..=MAX_CELL_PX as f32).contains(&cw) || !(1.0..=MAX_CELL_PX as f32).contains(&ch) {
            return Ok(false);
        }
        let (cw, ch) = (cw as u32, ch as u32);

        // Device step vectors (linear part of `t` applied to the pattern-space
        // step vectors). For an axis-aligned matrix the cross terms are ~0.
        let step_x = (x_step * t.sx, x_step * t.ky);
        let step_y = (y_step * t.kx, y_step * t.sy);
        let scale =
            t.sx.abs()
                .max(t.sy.abs())
                .max(t.kx.abs())
                .max(t.ky.abs())
                .max(1e-6);
        let axis_aligned = t.kx.abs() <= 1e-3 * scale && t.ky.abs() <= 1e-3 * scale;
        let step_x_len = step_x.0.hypot(step_x.1);
        let step_y_len = step_y.0.hypot(step_y.1);

        // Render one cell into an offscreen pixmap sized to the device /BBox.
        let stream_data = if let Some(r) = pattern_ref {
            doc.decode_stream_with_encryption(&pattern_obj, r)?
        } else {
            pattern_obj.decode_stream_data()?
        };
        let cell_ops = match parse_content_stream(&stream_data) {
            Ok(ops) => ops,
            Err(_) => return Ok(false),
        };
        let mut cell = match Pixmap::new(cw, ch) {
            Some(p) => p,
            None => return Ok(false),
        };
        // Map pattern space into the cell pixmap: `t`, shifted so the cell's
        // device min-corner lands at the pixmap origin.
        let cell_transform = Transform::from_translate(-cminx, -cminy).pre_concat(t);
        // Render the cell with a fresh resource scope and no CMYK sidecar
        // (the sidecar is sized to the page pixmap, not this cell).
        let saved_sidecar = self.cmyk_sidecar.take();
        let saved_fonts = self.fonts.clone();
        let saved_cs = self.color_spaces.clone();
        let _ = self.load_resources(doc, &pattern_resources);
        let render_res = self.execute_operators(
            &mut cell,
            cell_transform,
            &cell_ops,
            doc,
            page_num,
            &pattern_resources,
        );
        self.fonts = saved_fonts;
        self.color_spaces = saved_cs;
        self.cmyk_sidecar = saved_sidecar;
        if render_res.is_err() {
            return Ok(false);
        }

        // /PaintType 2 (uncoloured): recolour the cell coverage with the
        // current fill colour, preserving the rendered alpha.
        if paint_type == 2 {
            let (fr, fg, fb) = gs.fill_color_rgb;
            let (fr, fg, fb) = (
                (fr.clamp(0.0, 1.0) * 255.0) as u32,
                (fg.clamp(0.0, 1.0) * 255.0) as u32,
                (fb.clamp(0.0, 1.0) * 255.0) as u32,
            );
            for px in cell.data_mut().chunks_exact_mut(4) {
                let a = px[3] as u32;
                px[0] = (fr * a / 255) as u8;
                px[1] = (fg * a / 255) as u8;
                px[2] = (fb * a / 255) as u8;
            }
        }

        // Average (premultiplied) cell colour, used both for the geometry
        // fallback and to skip fully-transparent cells.
        let (mut sr, mut sg, mut sb, mut sa) = (0u64, 0u64, 0u64, 0u64);
        for px in cell.data().chunks_exact(4) {
            sr += px[0] as u64;
            sg += px[1] as u64;
            sb += px[2] as u64;
            sa += px[3] as u64;
        }
        let npix = (cw as u64) * (ch as u64);
        let avg_a = (sa / npix) as u8;
        if avg_a == 0 && paint_type == 1 {
            // Nothing visible in the cell — region stays as the backdrop.
            return Ok(true);
        }
        // Un-premultiply the average to a straight colour for the flood path.
        let unpremul = |sum: u64| -> u8 {
            if avg_a == 0 {
                0
            } else {
                (((sum / npix) as f32) * 255.0 / avg_a as f32).min(255.0) as u8
            }
        };
        let avg_color =
            tiny_skia::Color::from_rgba8(unpremul(sr), unpremul(sg), unpremul(sb), avg_a);

        // Device-space region to cover: the fill path's bounds mapped through
        // `path_transform`, clamped to the pixmap.
        let b = path.bounds();
        let pm = |x: f32, y: f32| -> (f32, f32) {
            (
                x * path_transform.sx + y * path_transform.kx + path_transform.tx,
                x * path_transform.ky + y * path_transform.sy + path_transform.ty,
            )
        };
        let pcorners = [
            pm(b.left(), b.top()),
            pm(b.right(), b.top()),
            pm(b.right(), b.bottom()),
            pm(b.left(), b.bottom()),
        ];
        let (mut rx0, mut ry0, mut rx1, mut ry1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for (x, y) in pcorners {
            rx0 = rx0.min(x);
            ry0 = ry0.min(y);
            rx1 = rx1.max(x);
            ry1 = ry1.max(y);
        }
        let (w, h) = (pixmap.width() as f32, pixmap.height() as f32);
        rx0 = rx0.max(0.0);
        ry0 = ry0.max(0.0);
        rx1 = rx1.min(w);
        ry1 = ry1.min(h);
        if rx1 <= rx0 || ry1 <= ry0 {
            return Ok(true); // fill region off-screen — nothing to paint
        }

        // Geometry guards: rotated/sheared matrix, degenerate or too-dense
        // steps, or an unusable tile count -> flood the path with the cell's
        // average colour instead of tiling.
        let mut base_paint = tiny_skia::Paint::default();
        base_paint.anti_alias = true;
        let flood = |pixmap: &mut Pixmap| {
            let mut p = base_paint.clone();
            p.set_color(avg_color);
            pixmap.fill_path(path, &p, fill_rule, path_transform, clip);
        };
        if !axis_aligned
            || x_step.abs() <= f32::EPSILON
            || y_step.abs() <= f32::EPSILON
            || step_x_len < 0.5
            || step_y_len < 0.5
        {
            flood(pixmap);
            return Ok(true);
        }

        let (i_lo, i_hi) = axis_tile_range(rx0, rx1, cminx, cw as f32, step_x.0);
        let (j_lo, j_hi) = axis_tile_range(ry0, ry1, cminy, ch as f32, step_y.1);
        let tile_count = (i_hi as i64 - i_lo as i64 + 1) * (j_hi as i64 - j_lo as i64 + 1);
        if tile_count <= 0 || tile_count > MAX_TILES {
            flood(pixmap);
            return Ok(true);
        }

        // Build the fill-region mask (path coverage ∩ active clip) once and
        // blit the cell into every lattice position under it.
        let mut mask = match tiny_skia::Mask::new(pixmap.width(), pixmap.height()) {
            Some(m) => m,
            None => {
                flood(pixmap);
                return Ok(true);
            }
        };
        mask.fill_path(path, fill_rule, true, path_transform);
        if let Some(c) = clip {
            for (mv, cv) in mask.data_mut().iter_mut().zip(c.data().iter()) {
                *mv = (*mv).min(*cv);
            }
        }

        let blit = PixmapPaint {
            opacity: gs.fill_alpha.clamp(0.0, 1.0),
            // Nearest keeps tile seams crisp for axis-aligned integer-ish steps.
            quality: tiny_skia::FilterQuality::Nearest,
            ..PixmapPaint::default()
        };
        for j in j_lo..=j_hi {
            for i in i_lo..=i_hi {
                let px = cminx + i as f32 * step_x.0 + j as f32 * step_y.0;
                let py = cminy + i as f32 * step_x.1 + j as f32 * step_y.1;
                pixmap.draw_pixmap(
                    0,
                    0,
                    cell.as_ref(),
                    &blit,
                    Transform::from_translate(px, py),
                    Some(&mask),
                );
            }
        }
        Ok(true)
    }
}
