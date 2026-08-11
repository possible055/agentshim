use super::*;

impl PageRenderer {
    /// Render a shading pattern (gradient).
    pub(super) fn render_shading(
        &self,
        pixmap: &mut Pixmap,
        name: &str,
        transform: Transform,
        gs: &GraphicsState,
        resources: &Object,
        doc: &PdfDocument,
        clip_mask: Option<&tiny_skia::Mask>,
    ) -> Result<()> {
        // Look up shading resource. Retain the full resolved object — for
        // mesh shadings (Types 4-7) the geometry lives in the object's
        // stream body, which `as_dict()` alone would drop.
        let shading_obj = if let Object::Dictionary(res_dict) = resources {
            if let Some(shading_res) = res_dict.get("Shading") {
                let resolved = doc.resolve_object(shading_res)?;
                if let Some(shadings) = resolved.as_dict() {
                    if let Some(sh_obj) = shadings.get(name) {
                        Some(doc.resolve_object(sh_obj)?)
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };

        let shading = match shading_obj.as_ref().and_then(|o| o.as_dict()) {
            Some(d) => d.clone(),
            None => {
                log::debug!("Shading '{}' not found in resources", name);
                return Ok(());
            }
        };

        let shading_type = shading
            .get("ShadingType")
            .and_then(|o| o.as_integer())
            .unwrap_or(0);

        // Pre-resolve gradient endpoint colours through the resolution
        // pipeline for the shading types we migrate (axial=2, radial=3).
        // For both types the endpoint
        // colours live in the shading's `/Function` (Type 2 exponential
        // interpolation puts the endpoints directly in `/C0` and
        // `/C1`; Type 3 stitching wraps a sub-function whose first /
        // last sub-functions carry them). The current inline path reads
        // `/C0` and `/C1` raw and treats them as already-RGB, which
        // silently truncates DeviceCMYK to its first three components
        // and drops Separation tint-transform evaluation entirely. The
        // pipeline-resolved endpoints respect the shading dict's
        // `/ColorSpace`, so a Type 4 Separation `/C0` becomes the
        // function's actual output rather than a `1 - tint` fall-back.
        //
        // Types 1 (function-based) and 4-7 (mesh) carry per-point /
        // per-vertex colours, not endpoints; this wave does NOT migrate
        // them. They fall straight through to the existing inline path,
        // unmodified.
        let resolved_endpoints = if shading_type == 2 || shading_type == 3 {
            self.pipeline_resolve_shading_endpoints(&shading, gs, doc)
        } else {
            None
        };

        match shading_type {
            2 => self.render_axial_shading(
                pixmap,
                &shading,
                transform,
                gs,
                clip_mask,
                resolved_endpoints,
            ),
            3 => self.render_radial_shading(
                pixmap,
                &shading,
                transform,
                gs,
                clip_mask,
                resolved_endpoints,
            ),
            1 | 4 | 5 | 6 | 7 => {
                // Mesh (Types 4-7) and function-based (Type 1) shadings are
                // rasterised by the dedicated hand-written backend — they do
                // not map onto a tiny-skia gradient shader. Colours read from
                // the geometry stream (or produced by the shading's optional
                // `/Function`) are routed back through the standard §8.6
                // colour-space resolution path via this closure so DeviceN /
                // Separation / ICCBased colour spaces resolve identically to
                // the axial/radial endpoints.
                let shading_obj = match shading_obj.as_ref() {
                    Some(o) => o,
                    None => return Ok(()),
                };
                let resolved_cs = shading
                    .get("ColorSpace")
                    .and_then(|o| doc.resolve_object(o).ok());
                let resolve_color = |comps: &[f32]| -> Option<(f32, f32, f32, f32)> {
                    let cs = resolved_cs.as_ref()?;
                    self.pipeline_resolve_components(
                        doc,
                        &self.color_spaces,
                        cs,
                        comps,
                        gs.fill_alpha,
                    )
                };
                crate::rendering::mesh_shading::render_mesh_shading(
                    pixmap,
                    &shading,
                    shading_obj,
                    shading_type,
                    transform,
                    doc,
                    clip_mask,
                    &resolve_color,
                )
            }
            _ => {
                log::debug!("Unsupported shading type {} for '{}'", shading_type, name);
                Ok(())
            }
        }
    }

    /// Resolve a Type 2 / Type 3 shading dictionary's `/C0` and `/C1`
    /// endpoint colours through the resolution pipeline. The shading
    /// dict's `/ColorSpace` selects the colour space; `/Function` (a
    /// Type 2 exponential or a Type 3 stitching wrapper) carries the
    /// endpoint component arrays. Returns `None` when either endpoint
    /// can't be resolved (missing `/Function`, unsupported sub-function
    /// type, non-RGBA resolver output, etc.) — the caller falls back to
    /// the existing inline behaviour in that case.
    ///
    /// Splits the "what colour" decision (pipeline-resolved) from the
    /// "how to interpolate" decision (still owned by the gradient
    /// backend). The interpolation math is untouched — only the two
    /// fixed endpoint colours are routed through the pipeline.
    pub(super) fn pipeline_resolve_shading_endpoints(
        &self,
        shading: &std::collections::HashMap<String, Object>,
        gs: &GraphicsState,
        doc: &PdfDocument,
    ) -> Option<((f32, f32, f32, f32), (f32, f32, f32, f32))> {
        // The shading dict's `/ColorSpace` can be a Name (DeviceRGB,
        // CS1, ...) or an inline Array ([/Separation ... funcRef]).
        // Resolve indirect references so the helper sees the final
        // shape.
        let cs_obj = shading.get("ColorSpace")?;
        let resolved_cs = doc.resolve_object(cs_obj).ok()?;

        // Per ISO 32000-1 §8.7.4.5.3, axial/radial shadings carry a
        // `/Domain` array on the shading dict (default `[0 1]`) that
        // names the parameter range mapped to the gradient axis.
        // Geometric `t=0` evaluates the function at `Domain[0]` and
        // `t=1` evaluates it at `Domain[1]` — the endpoints aren't
        // necessarily `f(0)` and `f(1)`.
        let (domain0, domain1) = shading
            .get("Domain")
            .and_then(|o| o.as_array())
            .and_then(|arr| {
                let d0 = arr.first()?;
                let d1 = arr.get(1)?;
                let parse = |o: &Object| -> Option<f32> {
                    match o {
                        Object::Real(v) => Some(*v as f32),
                        Object::Integer(v) => Some(*v as f32),
                        _ => None,
                    }
                };
                Some((parse(d0)?, parse(d1)?))
            })
            .unwrap_or((0.0, 1.0));

        // Extract endpoint component arrays from `/Function`. Handles
        // Type 2 (exponential) — where the endpoints are evaluated by
        // applying the shading's `/Domain` to the function's
        // exponential interpolation — and Type 3 (stitching) — where
        // the first sub-function's `/C0` and the last sub-function's
        // `/C1` are taken at face value. Type 3 with non-trivial
        // `/Encode` is not honoured; see the body comment below.
        let func_obj = shading.get("Function")?;
        let resolved_func = doc.resolve_object(func_obj).ok()?;
        let func_dict = resolved_func.as_dict()?;
        let func_type = func_dict.get("FunctionType").and_then(|o| o.as_integer())?;
        let to_components = |arr: &[Object]| -> Vec<f32> {
            arr.iter()
                .map(|o| match o {
                    Object::Real(v) => *v as f32,
                    Object::Integer(v) => *v as f32,
                    _ => 0.0,
                })
                .collect()
        };
        let (c0_comps, c1_comps) = match func_type {
            2 => {
                // Type 2: exponential interpolation
                // f(x) = C0 + x^N * (C1 - C0).
                // The shading's geometric `t=0` evaluates `f(Domain[0])`
                // and `t=1` evaluates `f(Domain[1])`, so when /Domain
                // is non-default the endpoint colours are NOT raw /C0
                // and /C1.
                let c0 = to_components(func_dict.get("C0").and_then(|o| o.as_array())?);
                let c1 = to_components(func_dict.get("C1").and_then(|o| o.as_array())?);
                let n = func_dict
                    .get("N")
                    .and_then(|o| match o {
                        Object::Real(v) => Some(*v as f32),
                        Object::Integer(v) => Some(*v as f32),
                        _ => None,
                    })
                    .unwrap_or(1.0);
                let eval = |x: f32| -> Vec<f32> {
                    let p = x.abs().powf(n) * x.signum();
                    c0.iter()
                        .zip(c1.iter())
                        .map(|(a, b)| *a + p * (*b - *a))
                        .collect()
                };
                (eval(domain0), eval(domain1))
            }
            3 => {
                // Type 3: stitching. The shading's `/Domain` maps to a
                // sub-function via stitching `/Bounds` and `/Encode`
                // arrays. The current path takes the first
                // sub-function's `/C0` and the last sub-function's
                // `/C1` at face value — correct for the default
                // `Domain [0 1]` with natural `Encode`, but ignores
                // `Encode`-driven sub-domain remapping. Documented gap.
                let funcs = func_dict.get("Functions").and_then(|o| o.as_array())?;
                let first = funcs.first()?;
                let last = funcs.last().unwrap_or(first);
                let first_resolved = doc.resolve_object(first).ok()?;
                let last_resolved = doc.resolve_object(last).ok()?;
                let first_dict = first_resolved.as_dict()?;
                let last_dict = last_resolved.as_dict()?;
                let c0 = first_dict.get("C0").and_then(|o| o.as_array())?;
                let c1 = last_dict.get("C1").and_then(|o| o.as_array())?;
                (to_components(c0), to_components(c1))
            }
            // Function types 0 (sampled) and 4 (PostScript Type 4
            // calculator) used as the shading's own /Function are
            // out-of-scope for endpoint pre-resolution — they produce
            // colours at intermediate domain points, not at two fixed
            // /C0 / /C1 arrays. Caller falls back to inline.
            _ => return None,
        };

        // Fold in `gs.fill_alpha` here — it's the alpha the inline
        // code path multiplies into each gradient stop's RGBA when
        // building the tiny-skia LinearGradient / RadialGradient.
        let c0 = self.pipeline_resolve_components(
            doc,
            &self.color_spaces,
            &resolved_cs,
            &c0_comps,
            gs.fill_alpha,
        )?;
        let c1 = self.pipeline_resolve_components(
            doc,
            &self.color_spaces,
            &resolved_cs,
            &c1_comps,
            gs.fill_alpha,
        )?;
        Some((c0, c1))
    }

    /// Render axial (linear) gradient shading (Type 2).
    ///
    /// `resolved_endpoints`, when `Some`, supplies pre-resolved RGBA
    /// values for the two gradient stops with `gs.fill_alpha` already
    /// folded in — the resolution-pipeline route produced by
    /// [`Self::pipeline_resolve_shading_endpoints`]. When `None`, the
    /// function falls back to a black-to-white default
    /// (the safety net the legacy inline path used as its outermost
    /// fallback before wave 5).
    pub(super) fn render_axial_shading(
        &self,
        pixmap: &mut Pixmap,
        shading: &std::collections::HashMap<String, Object>,
        transform: Transform,
        gs: &GraphicsState,
        clip_mask: Option<&tiny_skia::Mask>,
        resolved_endpoints: Option<((f32, f32, f32, f32), (f32, f32, f32, f32))>,
    ) -> Result<()> {
        // Parse Coords [x0 y0 x1 y1]
        let coords = shading.get("Coords").and_then(|o| o.as_array());
        let coords = match coords {
            Some(c) if c.len() >= 4 => c,
            _ => return Ok(()),
        };
        let get_f = |i: usize| -> f32 {
            match &coords[i] {
                Object::Real(v) => *v as f32,
                Object::Integer(v) => *v as f32,
                _ => 0.0,
            }
        };
        let (x0, y0, x1, y1) = (get_f(0), get_f(1), get_f(2), get_f(3));

        // Parse Extend [bool bool]
        let extend = shading.get("Extend").and_then(|o| o.as_array());
        let (extend_start, extend_end) = if let Some(ext) = extend {
            let e0 = ext
                .get(0)
                .map(|o| matches!(o, Object::Boolean(true)))
                .unwrap_or(false);
            let e1 = ext
                .get(1)
                .map(|o| matches!(o, Object::Boolean(true)))
                .unwrap_or(false);
            (e0, e1)
        } else {
            (false, false)
        };

        // Build the two gradient-stop RGBAs from the pipeline's
        // pre-resolved endpoint pair. When the resolver cannot produce
        // an answer (missing /Function, unsupported sub-function type,
        // non-RGBA resolver output) fall back to the
        // black-to-white default that matches the legacy renderer's
        // safety net — render with sensible defaults rather than
        // panicking or rendering nothing.
        let (stop0, stop1) = match resolved_endpoints {
            Some(((r0, g0, b0, a0), (r1, g1, b1, a1))) => ((r0, g0, b0, a0), (r1, g1, b1, a1)),
            None => (
                (0.0, 0.0, 0.0, gs.fill_alpha),
                (1.0, 1.0, 1.0, gs.fill_alpha),
            ),
        };

        // Transform gradient endpoints
        let mut p0 = tiny_skia::Point { x: x0, y: y0 };
        let mut p1 = tiny_skia::Point { x: x1, y: y1 };
        transform.map_point(&mut p0);
        transform.map_point(&mut p1);

        // Per ISO 32000-1 §8.7.4.5.3 the `/Extend` array names whether
        // the gradient paints past its geometric endpoints with the
        // adjacent stop colour. tiny-skia's `SpreadMode::Pad` is the
        // `[true true]` behaviour. For the other three combinations
        // the area past the unwanted side must not be painted at all,
        // so we build an extra clip path from the gradient slab and
        // intersect it with the inherited `clip_mask`.
        let spread = tiny_skia::SpreadMode::Pad;

        // Build an axis-perpendicular slab clip when at least one side
        // is `false`. The slab is the strip between the two
        // perpendicular lines through `p0` and `p1`; for asymmetric
        // `/Extend`, one side of the strip is the page boundary, the
        // other is the perpendicular.
        let slab_clip_mask =
            build_axial_extend_clip(pixmap, p0, p1, extend_start, extend_end, clip_mask);
        let effective_clip = slab_clip_mask.as_ref().or(clip_mask);

        let gradient = tiny_skia::LinearGradient::new(
            tiny_skia::Point { x: p0.x, y: p0.y },
            tiny_skia::Point { x: p1.x, y: p1.y },
            vec![
                tiny_skia::GradientStop::new(
                    0.0,
                    tiny_skia::Color::from_rgba(stop0.0, stop0.1, stop0.2, stop0.3)
                        .unwrap_or(tiny_skia::Color::BLACK),
                ),
                tiny_skia::GradientStop::new(
                    1.0,
                    tiny_skia::Color::from_rgba(stop1.0, stop1.1, stop1.2, stop1.3)
                        .unwrap_or(tiny_skia::Color::BLACK),
                ),
            ],
            spread,
            Transform::identity(),
        );

        if let Some(shader) = gradient {
            let mut paint = tiny_skia::Paint::default();
            paint.shader = shader;
            paint.anti_alias = true;

            // Fill entire pixmap with gradient (clipped by clip_mask)
            let rect =
                tiny_skia::Rect::from_xywh(0.0, 0.0, pixmap.width() as f32, pixmap.height() as f32)
                    .unwrap();
            let path = PathBuilder::from_rect(rect);
            pixmap.fill_path(
                &path,
                &paint,
                tiny_skia::FillRule::Winding,
                Transform::identity(),
                effective_clip,
            );
            log::debug!(
                "Rendered axial gradient from ({:.1},{:.1}) to ({:.1},{:.1})",
                p0.x,
                p0.y,
                p1.x,
                p1.y
            );
        }

        Ok(())
    }

    /// Render radial gradient shading (Type 3).
    ///
    /// `resolved_endpoints`, when `Some`, supplies pre-resolved RGBA
    /// values for the two gradient stops with `gs.fill_alpha` already
    /// folded in — the resolution-pipeline route produced by
    /// [`Self::pipeline_resolve_shading_endpoints`]. When `None`, the
    /// function falls back to a black-to-white default (the safety net
    /// the legacy inline path used as its outermost fallback before
    /// wave 5).
    pub(super) fn render_radial_shading(
        &self,
        pixmap: &mut Pixmap,
        shading: &std::collections::HashMap<String, Object>,
        transform: Transform,
        gs: &GraphicsState,
        clip_mask: Option<&tiny_skia::Mask>,
        resolved_endpoints: Option<((f32, f32, f32, f32), (f32, f32, f32, f32))>,
    ) -> Result<()> {
        // Parse Coords [x0 y0 r0 x1 y1 r1]
        let coords = shading.get("Coords").and_then(|o| o.as_array());
        let coords = match coords {
            Some(c) if c.len() >= 6 => c,
            _ => return Ok(()),
        };
        let get_f = |i: usize| -> f32 {
            match &coords[i] {
                Object::Real(v) => *v as f32,
                Object::Integer(v) => *v as f32,
                _ => 0.0,
            }
        };
        let (x0, y0, r0, x1, y1, r1) = (get_f(0), get_f(1), get_f(2), get_f(3), get_f(4), get_f(5));

        // Parse Extend [bool bool] — same shape as the axial case.
        let extend = shading.get("Extend").and_then(|o| o.as_array());
        let (extend_start, extend_end) = if let Some(ext) = extend {
            let e0 = ext
                .first()
                .map(|o| matches!(o, Object::Boolean(true)))
                .unwrap_or(false);
            let e1 = ext
                .get(1)
                .map(|o| matches!(o, Object::Boolean(true)))
                .unwrap_or(false);
            (e0, e1)
        } else {
            (false, false)
        };

        // Same pipeline-or-fallback dispatch as `render_axial_shading`
        // — see its docs for the rationale.
        let (stop0, stop1) = match resolved_endpoints {
            Some(((r0c, g0, b0, a0), (r1c, g1, b1, a1))) => ((r0c, g0, b0, a0), (r1c, g1, b1, a1)),
            None => (
                (0.0, 0.0, 0.0, gs.fill_alpha),
                (1.0, 1.0, 1.0, gs.fill_alpha),
            ),
        };

        // Per ISO 32000-1 §8.7.4.5.4, the radial gradient interpolates
        // between two circles `(x0, y0, r0)` (the inner / start circle,
        // mapped to the function value at the gradient's `Domain[0]`)
        // and `(x1, y1, r1)` (the outer / end circle, mapped to
        // `Domain[1]`). When `(x0, y0) == (x1, y1)` and `r0 == 0` the
        // result is a familiar centred radial; non-concentric inputs
        // produce off-centre / cone gradients that real PDFs use for
        // highlight, spotlight, and lens effects.
        let mut center0 = tiny_skia::Point { x: x0, y: y0 };
        let mut edge0 = tiny_skia::Point { x: x0 + r0, y: y0 };
        let mut center1 = tiny_skia::Point { x: x1, y: y1 };
        let mut edge1 = tiny_skia::Point { x: x1 + r1, y: y1 };
        transform.map_point(&mut center0);
        transform.map_point(&mut edge0);
        transform.map_point(&mut center1);
        transform.map_point(&mut edge1);
        let radius0 = ((edge0.x - center0.x).powi(2) + (edge0.y - center0.y).powi(2)).sqrt();
        let radius1 = ((edge1.x - center1.x).powi(2) + (edge1.y - center1.y).powi(2)).sqrt();

        // Per ISO 32000-1 §8.7.4.5.4 the `/Extend` array names whether
        // the gradient paints past the start (inner) and end (outer)
        // circles with the adjacent stop colour. tiny-skia's
        // `SpreadMode::Pad` is the `[true true]` behaviour; for any
        // `false` side we need an explicit clip. For the common
        // `r0 < r1` case `Extend[1]=false` clips outside the outer
        // circle and `Extend[0]=false` clips inside the inner circle.
        let radial_clip_mask = build_radial_extend_clip(
            pixmap,
            (center0, radius0),
            (center1, radius1),
            extend_start,
            extend_end,
            clip_mask,
        );
        let effective_clip = radial_clip_mask.as_ref().or(clip_mask);

        let gradient = tiny_skia::RadialGradient::new(
            tiny_skia::Point {
                x: center0.x,
                y: center0.y,
            },
            radius0, // start_radius (inner circle, in device space)
            tiny_skia::Point {
                x: center1.x,
                y: center1.y,
            },
            radius1, // end_radius (outer circle, in device space)
            vec![
                tiny_skia::GradientStop::new(
                    0.0,
                    tiny_skia::Color::from_rgba(stop0.0, stop0.1, stop0.2, stop0.3)
                        .unwrap_or(tiny_skia::Color::BLACK),
                ),
                tiny_skia::GradientStop::new(
                    1.0,
                    tiny_skia::Color::from_rgba(stop1.0, stop1.1, stop1.2, stop1.3)
                        .unwrap_or(tiny_skia::Color::BLACK),
                ),
            ],
            tiny_skia::SpreadMode::Pad,
            Transform::identity(),
        );

        if let Some(shader) = gradient {
            let mut paint = tiny_skia::Paint::default();
            paint.shader = shader;
            paint.anti_alias = true;
            let rect =
                tiny_skia::Rect::from_xywh(0.0, 0.0, pixmap.width() as f32, pixmap.height() as f32)
                    .unwrap();
            let path = PathBuilder::from_rect(rect);
            pixmap.fill_path(
                &path,
                &paint,
                tiny_skia::FillRule::Winding,
                Transform::identity(),
                effective_clip,
            );
            log::debug!(
                "Rendered radial gradient from ({:.1},{:.1}) r={:.1} to ({:.1},{:.1}) r={:.1}",
                center0.x,
                center0.y,
                radius0,
                center1.x,
                center1.y,
                radius1,
            );
        }

        Ok(())
    }
}
