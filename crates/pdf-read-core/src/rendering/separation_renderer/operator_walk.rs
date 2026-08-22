use super::*;

/// Execute operators for separation plate rendering, dispatching paint
/// operations to **all** target inks in parallel.
///
/// `pixmaps` and `target_inks` are parallel slices: `pixmaps[i]` receives
/// paint for ink `target_inks[i]`. The operator stream is walked exactly
/// once; every paint site (fill, stroke, text, Form XObject) iterates the
/// pair list and contributes to each plate whose ink the current colour
/// touches.
#[allow(clippy::too_many_arguments)]
pub(super) fn execute_separation_operators(
    pixmaps: &mut [Pixmap],
    base_transform: Transform,
    operators: &[Operator],
    ctx: &mut SeparationContext<'_>,
    resources: &Object,
    color_spaces: &HashMap<String, Object>,
    inherited: Option<&InheritedState>,
    target_inks: &[&str],
) -> Result<()> {
    debug_assert_eq!(pixmaps.len(), target_inks.len());
    let mut gs_stack = GraphicsStateStack::new();
    {
        let gs = gs_stack.current_mut();
        if let Some(inherited) = inherited {
            gs.fill_color_space = inherited.fill_color_space.clone();
            gs.stroke_color_space = inherited.stroke_color_space.clone();
            gs.fill_color_cmyk = inherited.fill_color_cmyk;
            gs.stroke_color_cmyk = inherited.stroke_color_cmyk;
            // §8.10.1: inherit the caller's overprint state too. Without
            // this, an outer `gs` setting OP=true would be silently
            // dropped at the Form XObject boundary and the form's CMYK
            // content would knock out underlying inks against the
            // caller's intent.
            gs.fill_overprint = inherited.fill_overprint;
            gs.stroke_overprint = inherited.stroke_overprint;
            gs.overprint_mode = inherited.overprint_mode;
        } else {
            gs.fill_color_space = "DeviceGray".to_string();
            gs.stroke_color_space = "DeviceGray".to_string();
        }
        gs.fill_color_rgb = (0.0, 0.0, 0.0);
        gs.stroke_color_rgb = (0.0, 0.0, 0.0);
    }

    let initial_cs = if let Some(inherited) = inherited {
        SeparationColorState {
            fill_components: inherited.fill_components.clone(),
            stroke_components: inherited.stroke_components.clone(),
        }
    } else {
        SeparationColorState::new()
    };
    let mut color_state_stack: Vec<SeparationColorState> = vec![initial_cs];
    let mut current_path = PathBuilder::new();
    let mut pending_clip: Option<(tiny_skia::Path, FillRule)> = None;
    let mut clip_stack: Vec<Option<Mask>> = vec![None];
    let mut in_text_object = false;

    // Pre-resolve ExtGState for the gs cache.
    let ext_g_state_resolved: Option<Object> = match resources {
        Object::Dictionary(rd) => rd
            .get("ExtGState")
            .and_then(|o| ctx.doc.resolve_object(o).ok()),
        _ => None,
    };
    let ext_g_states: Option<&HashMap<String, Object>> =
        ext_g_state_resolved.as_ref().and_then(|o| o.as_dict());
    let mut ext_g_state_cache: HashMap<String, ParsedExtGState> = HashMap::new();

    let xobjects_resolved: Option<Object> = match resources {
        Object::Dictionary(rd) => rd
            .get("XObject")
            .and_then(|o| ctx.doc.resolve_object(o).ok()),
        _ => None,
    };

    // Pixmap extent — every plate shares the same dimensions because they
    // all originate from a single allocation in `render_plates_for_inks`.
    // If `pixmaps` is empty (no inks to render), use a zero extent; the
    // operator walk still progresses for graphics-state tracking but
    // paint loops are no-ops because there are no targets.
    let pixmap_width = pixmaps.first().map(|p| p.width()).unwrap_or(0);
    let pixmap_height = pixmaps.first().map(|p| p.height()).unwrap_or(0);

    // Pipeline-driven dispatch state. The pipeline replaces the inline
    // `tint_for_ink` decision tree for Separation / DeviceN / ICCBased
    // sources — it's the only path that evaluates Type-4 tint
    // transforms, honours §8.6.6.3 `/All` and `/None`, and routes via
    // the §11.7.4 / §11.7.4.3 InkRouter rules. Process colour direct
    // (DeviceCMYK / DeviceGray) and DeviceRGB keep the inline fast
    // path because the inline arms are already correct for those.
    let pipeline = ResolutionPipeline::new();
    let mut backend = SeparationBackend::new();
    let target_inks_owned: Vec<InkName> = target_inks.iter().map(|s| InkName::new(*s)).collect();

    for op in operators {
        match op {
            Operator::SaveState => {
                gs_stack.save();
                let cs = color_state_stack
                    .last()
                    .cloned()
                    .unwrap_or_else(SeparationColorState::new);
                color_state_stack.push(cs);
                clip_stack.push(clip_stack.last().cloned().unwrap_or(None));
            }
            Operator::RestoreState => {
                gs_stack.restore();
                if color_state_stack.len() > 1 {
                    color_state_stack.pop();
                }
                if clip_stack.len() > 1 {
                    clip_stack.pop();
                }
            }

            Operator::Cm { a, b, c, d, e, f } => {
                let current = gs_stack.current_mut();
                let new_matrix = Matrix {
                    a: *a,
                    b: *b,
                    c: *c,
                    d: *d,
                    e: *e,
                    f: *f,
                };
                current.ctm = new_matrix.multiply(&current.ctm);
            }

            Operator::SetFillRgb { r, g, b } => {
                let gs = gs_stack.current_mut();
                gs.fill_color_rgb = (*r, *g, *b);
                gs.fill_color_space = "DeviceRGB".to_string();
                gs.fill_color_cmyk = None;
                if let Some(cs) = color_state_stack.last_mut() {
                    cs.fill_components = vec![*r, *g, *b];
                }
            }
            Operator::SetStrokeRgb { r, g, b } => {
                let gs = gs_stack.current_mut();
                gs.stroke_color_rgb = (*r, *g, *b);
                gs.stroke_color_space = "DeviceRGB".to_string();
                gs.stroke_color_cmyk = None;
                if let Some(cs) = color_state_stack.last_mut() {
                    cs.stroke_components = vec![*r, *g, *b];
                }
            }
            Operator::SetFillGray { gray } => {
                let g = *gray;
                let gs = gs_stack.current_mut();
                gs.fill_color_rgb = (g, g, g);
                gs.fill_color_space = "DeviceGray".to_string();
                gs.fill_color_cmyk = None;
                if let Some(cs) = color_state_stack.last_mut() {
                    cs.fill_components = vec![g];
                }
            }
            Operator::SetStrokeGray { gray } => {
                let g = *gray;
                let gs = gs_stack.current_mut();
                gs.stroke_color_rgb = (g, g, g);
                gs.stroke_color_space = "DeviceGray".to_string();
                gs.stroke_color_cmyk = None;
                if let Some(cs) = color_state_stack.last_mut() {
                    cs.stroke_components = vec![g];
                }
            }
            Operator::SetFillCmyk { c, m, y, k } => {
                let gs = gs_stack.current_mut();
                gs.fill_color_cmyk = Some((*c, *m, *y, *k));
                gs.fill_color_space = "DeviceCMYK".to_string();
                if let Some(cs) = color_state_stack.last_mut() {
                    cs.fill_components = vec![*c, *m, *y, *k];
                }
            }
            Operator::SetStrokeCmyk { c, m, y, k } => {
                let gs = gs_stack.current_mut();
                gs.stroke_color_cmyk = Some((*c, *m, *y, *k));
                gs.stroke_color_space = "DeviceCMYK".to_string();
                if let Some(cs) = color_state_stack.last_mut() {
                    cs.stroke_components = vec![*c, *m, *y, *k];
                }
            }
            Operator::SetFillColorSpace { name } => {
                let (components, cmyk) =
                    initial_components_for_space(name, color_spaces, resources, ctx.doc);
                let gs = gs_stack.current_mut();
                gs.fill_color_space = name.clone();
                gs.fill_color_cmyk = cmyk;
                if let Some(cs) = color_state_stack.last_mut() {
                    cs.fill_components = components;
                }
            }
            Operator::SetStrokeColorSpace { name } => {
                let (components, cmyk) =
                    initial_components_for_space(name, color_spaces, resources, ctx.doc);
                let gs = gs_stack.current_mut();
                gs.stroke_color_space = name.clone();
                gs.stroke_color_cmyk = cmyk;
                if let Some(cs) = color_state_stack.last_mut() {
                    cs.stroke_components = components;
                }
            }
            Operator::SetFillColor { components } | Operator::SetFillColorN { components, .. } => {
                let gs = gs_stack.current_mut();
                let space = gs.fill_color_space.clone();
                match space.as_str() {
                    "DeviceCMYK" | "CMYK" if components.len() >= 4 => {
                        gs.fill_color_cmyk =
                            Some((components[0], components[1], components[2], components[3]));
                    }
                    _ => {}
                }
                if let Some(cs) = color_state_stack.last_mut() {
                    cs.fill_components = components.clone();
                }
            }
            Operator::SetStrokeColor { components }
            | Operator::SetStrokeColorN { components, .. } => {
                let gs = gs_stack.current_mut();
                let space = gs.stroke_color_space.clone();
                match space.as_str() {
                    "DeviceCMYK" | "CMYK" if components.len() >= 4 => {
                        gs.stroke_color_cmyk =
                            Some((components[0], components[1], components[2], components[3]));
                    }
                    _ => {}
                }
                if let Some(cs) = color_state_stack.last_mut() {
                    cs.stroke_components = components.clone();
                }
            }

            Operator::SetLineWidth { width } => {
                gs_stack.current_mut().line_width = *width;
            }
            Operator::SetLineCap { cap_style } => {
                gs_stack.current_mut().line_cap = *cap_style;
            }
            Operator::SetLineJoin { join_style } => {
                gs_stack.current_mut().line_join = *join_style;
            }
            Operator::SetMiterLimit { limit } => {
                gs_stack.current_mut().miter_limit = *limit;
            }
            Operator::SetDash { array, phase } => {
                gs_stack.current_mut().dash_pattern = (array.clone(), *phase);
            }
            Operator::SetRenderingIntent { intent } => {
                // §10.7.3 — mirror the composite renderer's dispatch.
                // The per-plate path doesn't consult OutputIntent for
                // its CMYK channels (the plates ARE the press target),
                // but `gs.rendering_intent` still flows through the
                // resolver's ICCBased N=4 path, so keeping it current
                // matches the composite path's behaviour.
                gs_stack.current_mut().rendering_intent = intent.clone();
            }

            Operator::MoveTo { x, y } => {
                current_path.move_to(*x, *y);
            }
            Operator::LineTo { x, y } => {
                current_path.line_to(*x, *y);
            }
            Operator::CurveTo {
                x1,
                y1,
                x2,
                y2,
                x3,
                y3,
            } => {
                current_path.cubic_to(*x1, *y1, *x2, *y2, *x3, *y3);
            }
            Operator::CurveToV { x2, y2, x3, y3 } => {
                if let Some(last) = current_path.last_point() {
                    current_path.cubic_to(last.x, last.y, *x2, *y2, *x3, *y3);
                }
            }
            Operator::CurveToY { x1, y1, x3, y3 } => {
                current_path.cubic_to(*x1, *y1, *x3, *y3, *x3, *y3);
            }
            Operator::Rectangle {
                x,
                y,
                width,
                height,
            } => {
                let (nx, nw) = if *width < 0.0 {
                    (x + width, -width)
                } else {
                    (*x, *width)
                };
                let (ny, nh) = if *height < 0.0 {
                    (y + height, -height)
                } else {
                    (*y, *height)
                };
                if let Some(rect) = tiny_skia::Rect::from_xywh(nx, ny, nw, nh) {
                    current_path.push_rect(rect);
                }
            }
            Operator::ClosePath => {
                current_path.close();
            }

            Operator::Stroke => {
                apply_separation_clip(
                    &mut pending_clip,
                    &mut clip_stack,
                    pixmap_width,
                    pixmap_height,
                    base_transform,
                    &gs_stack,
                );
                if let Some(path) = current_path.finish() {
                    paint_path_to_plates(
                        &path,
                        None,
                        true,
                        pixmaps,
                        target_inks,
                        &target_inks_owned,
                        base_transform,
                        gs_stack.current(),
                        color_state_stack.last(),
                        color_spaces,
                        resources,
                        ctx.doc,
                        clip_stack.last().and_then(|clip| clip.as_ref()),
                        &pipeline,
                        &mut backend,
                    )?;
                }
                current_path = PathBuilder::new();
            }
            Operator::Fill => {
                apply_separation_clip(
                    &mut pending_clip,
                    &mut clip_stack,
                    pixmap_width,
                    pixmap_height,
                    base_transform,
                    &gs_stack,
                );
                if let Some(path) = current_path.finish() {
                    paint_path_to_plates(
                        &path,
                        Some(FillRule::Winding),
                        false,
                        pixmaps,
                        target_inks,
                        &target_inks_owned,
                        base_transform,
                        gs_stack.current(),
                        color_state_stack.last(),
                        color_spaces,
                        resources,
                        ctx.doc,
                        clip_stack.last().and_then(|clip| clip.as_ref()),
                        &pipeline,
                        &mut backend,
                    )?;
                }
                current_path = PathBuilder::new();
            }
            Operator::FillEvenOdd => {
                apply_separation_clip(
                    &mut pending_clip,
                    &mut clip_stack,
                    pixmap_width,
                    pixmap_height,
                    base_transform,
                    &gs_stack,
                );
                if let Some(path) = current_path.finish() {
                    paint_path_to_plates(
                        &path,
                        Some(FillRule::EvenOdd),
                        false,
                        pixmaps,
                        target_inks,
                        &target_inks_owned,
                        base_transform,
                        gs_stack.current(),
                        color_state_stack.last(),
                        color_spaces,
                        resources,
                        ctx.doc,
                        clip_stack.last().and_then(|clip| clip.as_ref()),
                        &pipeline,
                        &mut backend,
                    )?;
                }
                current_path = PathBuilder::new();
            }
            Operator::FillStroke | Operator::CloseFillStroke => {
                apply_separation_clip(
                    &mut pending_clip,
                    &mut clip_stack,
                    pixmap_width,
                    pixmap_height,
                    base_transform,
                    &gs_stack,
                );
                if let Some(path) = current_path.finish() {
                    paint_path_to_plates(
                        &path,
                        Some(FillRule::Winding),
                        true,
                        pixmaps,
                        target_inks,
                        &target_inks_owned,
                        base_transform,
                        gs_stack.current(),
                        color_state_stack.last(),
                        color_spaces,
                        resources,
                        ctx.doc,
                        clip_stack.last().and_then(|clip| clip.as_ref()),
                        &pipeline,
                        &mut backend,
                    )?;
                }
                current_path = PathBuilder::new();
            }
            Operator::FillStrokeEvenOdd | Operator::CloseFillStrokeEvenOdd => {
                apply_separation_clip(
                    &mut pending_clip,
                    &mut clip_stack,
                    pixmap_width,
                    pixmap_height,
                    base_transform,
                    &gs_stack,
                );
                if let Some(path) = current_path.finish() {
                    paint_path_to_plates(
                        &path,
                        Some(FillRule::EvenOdd),
                        true,
                        pixmaps,
                        target_inks,
                        &target_inks_owned,
                        base_transform,
                        gs_stack.current(),
                        color_state_stack.last(),
                        color_spaces,
                        resources,
                        ctx.doc,
                        clip_stack.last().and_then(|clip| clip.as_ref()),
                        &pipeline,
                        &mut backend,
                    )?;
                }
                current_path = PathBuilder::new();
            }
            Operator::EndPath => {
                apply_separation_clip(
                    &mut pending_clip,
                    &mut clip_stack,
                    pixmap_width,
                    pixmap_height,
                    base_transform,
                    &gs_stack,
                );
                current_path = PathBuilder::new();
            }

            Operator::ClipNonZero => {
                if let Some(path) = current_path.clone().finish() {
                    pending_clip = Some((path, FillRule::Winding));
                }
            }
            Operator::ClipEvenOdd => {
                if let Some(path) = current_path.clone().finish() {
                    pending_clip = Some((path, FillRule::EvenOdd));
                }
            }

            // Text object
            Operator::BeginText => {
                in_text_object = true;
                let gs = gs_stack.current_mut();
                gs.text_matrix = Matrix::identity();
                gs.text_line_matrix = Matrix::identity();
            }
            Operator::EndText => {
                in_text_object = false;
            }

            // Text state
            Operator::Tc { char_space } => {
                gs_stack.current_mut().char_space = *char_space;
            }
            Operator::Tw { word_space } => {
                gs_stack.current_mut().word_space = *word_space;
            }
            Operator::Tz { scale } => {
                gs_stack.current_mut().horizontal_scaling = *scale;
            }
            Operator::TL { leading } => {
                gs_stack.current_mut().leading = *leading;
            }
            Operator::Ts { rise } => {
                gs_stack.current_mut().text_rise = *rise;
            }
            Operator::Tr { render } => {
                gs_stack.current_mut().render_mode = *render;
            }
            Operator::Tf { font, size } => {
                let wmode = ctx.fonts.get(font).map(|f| f.wmode).unwrap_or(0);
                let gs = gs_stack.current_mut();
                gs.font_name = Some(font.clone());
                gs.font_size = *size;
                gs.text_wmode = wmode;
            }

            // Text positioning
            Operator::Td { tx, ty } => {
                if in_text_object {
                    let gs = gs_stack.current_mut();
                    let translation = Matrix::translation(*tx, *ty);
                    gs.text_line_matrix = translation.multiply(&gs.text_line_matrix);
                    gs.text_matrix = gs.text_line_matrix;
                }
            }
            Operator::TD { tx, ty } => {
                if in_text_object {
                    let gs = gs_stack.current_mut();
                    gs.leading = -(*ty);
                    let translation = Matrix::translation(*tx, *ty);
                    gs.text_line_matrix = translation.multiply(&gs.text_line_matrix);
                    gs.text_matrix = gs.text_line_matrix;
                }
            }
            Operator::Tm { a, b, c, d, e, f } => {
                if in_text_object {
                    let gs = gs_stack.current_mut();
                    gs.text_matrix = Matrix {
                        a: *a,
                        b: *b,
                        c: *c,
                        d: *d,
                        e: *e,
                        f: *f,
                    };
                    gs.text_line_matrix = gs.text_matrix;
                }
            }
            Operator::TStar => {
                if in_text_object {
                    let gs = gs_stack.current_mut();
                    let leading = gs.leading;
                    let translation = Matrix::translation(0.0, -leading);
                    gs.text_line_matrix = translation.multiply(&gs.text_line_matrix);
                    gs.text_matrix = gs.text_line_matrix;
                }
            }

            // Text showing. Each `render_*_to_plate` returns a scalar
            // advance along the active writing axis; the caller routes it
            // into the text matrix through `advance_text_matrix`, which
            // performs the H/V axis swap in one place.
            Operator::Tj { text } => {
                if in_text_object {
                    let advance = render_text_to_plate(
                        pixmaps,
                        text,
                        base_transform,
                        &mut gs_stack,
                        &color_state_stack,
                        color_spaces,
                        resources,
                        ctx,
                        clip_stack.last().and_then(|c| c.as_ref()),
                        target_inks,
                    )?;
                    gs_stack.current_mut().advance_text_matrix(advance);
                }
            }
            Operator::TJ { array } => {
                if in_text_object {
                    let advance = render_tj_to_plate(
                        pixmaps,
                        array,
                        base_transform,
                        &mut gs_stack,
                        &color_state_stack,
                        color_spaces,
                        resources,
                        ctx,
                        clip_stack.last().and_then(|c| c.as_ref()),
                        target_inks,
                    )?;
                    gs_stack.current_mut().advance_text_matrix(advance);
                }
            }
            Operator::Quote { text } => {
                if in_text_object {
                    let gs_mut = gs_stack.current_mut();
                    let leading = gs_mut.leading;
                    let translation = Matrix::translation(0.0, -leading);
                    gs_mut.text_line_matrix = translation.multiply(&gs_mut.text_line_matrix);
                    gs_mut.text_matrix = gs_mut.text_line_matrix;

                    let advance = render_text_to_plate(
                        pixmaps,
                        text,
                        base_transform,
                        &mut gs_stack,
                        &color_state_stack,
                        color_spaces,
                        resources,
                        ctx,
                        clip_stack.last().and_then(|c| c.as_ref()),
                        target_inks,
                    )?;
                    gs_stack.current_mut().advance_text_matrix(advance);
                }
            }
            Operator::DoubleQuote {
                word_space,
                char_space,
                text,
            } => {
                if in_text_object {
                    let gs_mut = gs_stack.current_mut();
                    gs_mut.word_space = *word_space;
                    gs_mut.char_space = *char_space;
                    let leading = gs_mut.leading;
                    let translation = Matrix::translation(0.0, -leading);
                    gs_mut.text_line_matrix = translation.multiply(&gs_mut.text_line_matrix);
                    gs_mut.text_matrix = gs_mut.text_line_matrix;

                    let advance = render_text_to_plate(
                        pixmaps,
                        text,
                        base_transform,
                        &mut gs_stack,
                        &color_state_stack,
                        color_spaces,
                        resources,
                        ctx,
                        clip_stack.last().and_then(|c| c.as_ref()),
                        target_inks,
                    )?;
                    gs_stack.current_mut().advance_text_matrix(advance);
                }
            }

            // ExtGState
            Operator::SetExtGState { dict_name } => {
                let entry = ext_g_state_cache
                    .entry(dict_name.clone())
                    .or_insert_with(|| {
                        if let Some(states) = ext_g_states {
                            if let Some(state_obj) = states.get(dict_name) {
                                return parse_ext_g_state_inner(state_obj, ctx.doc)
                                    .unwrap_or_default();
                            }
                        }
                        ParsedExtGState::default()
                    });
                entry.apply(gs_stack.current_mut());
            }

            // XObject — Form XObjects recurse into their content stream;
            // Image XObjects route per-channel samples to the matching ink
            // plates (§8.9, §11.7.4 default routing).
            Operator::Do { name } => {
                if let Some(xobjects) = xobjects_resolved.as_ref().and_then(|o| o.as_dict()) {
                    if let Some(xobj_ref_obj) = xobjects.get(name) {
                        if let Ok(xobj) = ctx.doc.resolve_object(xobj_ref_obj) {
                            if let Object::Stream { ref dict, .. } = xobj {
                                if let Some(subtype) = dict.get("Subtype").and_then(|o| o.as_name())
                                {
                                    if subtype == "Image" {
                                        let xobj_ref = xobj_ref_obj.as_reference();
                                        paint_image_to_plates(
                                            pixmaps,
                                            name,
                                            &xobj,
                                            xobj_ref,
                                            base_transform,
                                            &gs_stack,
                                            color_state_stack.last(),
                                            color_spaces,
                                            resources,
                                            ctx,
                                            clip_stack.last().and_then(|c| c.as_ref()),
                                            target_inks,
                                        )?;
                                    } else if subtype == "Form" {
                                        let xobj_ref = xobj_ref_obj.as_reference();
                                        let stream_data = if let Some(r) = xobj_ref {
                                            ctx.doc.decode_stream_with_encryption(&xobj, r)?
                                        } else {
                                            xobj.decode_stream_data()?
                                        };

                                        let form_resources =
                                            if let Some(res) = dict.get("Resources") {
                                                ctx.doc.resolve_object(res)?
                                            } else {
                                                resources.clone()
                                            };

                                        let form_cs = load_color_spaces(ctx.doc, &form_resources)?;
                                        let mut merged_cs = color_spaces.clone();
                                        merged_cs.extend(form_cs);

                                        let form_matrix = parse_form_matrix(dict);
                                        let gs = gs_stack.current();
                                        let combined = combine_transforms(base_transform, &gs.ctm)
                                            .pre_concat(form_matrix);

                                        // Inherit the calling context's colour state into the
                                        // form's initial graphics state (PDF §8.10.1, O5).
                                        let empty = SeparationColorState::new();
                                        let cs = color_state_stack.last().unwrap_or(&empty);
                                        let inherited = InheritedState {
                                            fill_color_space: gs.fill_color_space.clone(),
                                            stroke_color_space: gs.stroke_color_space.clone(),
                                            fill_color_cmyk: gs.fill_color_cmyk,
                                            stroke_color_cmyk: gs.stroke_color_cmyk,
                                            fill_components: cs.fill_components.clone(),
                                            stroke_components: cs.stroke_components.clone(),
                                            fill_overprint: gs.fill_overprint,
                                            stroke_overprint: gs.stroke_overprint,
                                            overprint_mode: gs.overprint_mode,
                                        };

                                        let form_ops = parse_content_stream(&stream_data)?;
                                        execute_separation_operators(
                                            pixmaps,
                                            combined,
                                            &form_ops,
                                            ctx,
                                            &form_resources,
                                            &merged_cs,
                                            Some(&inherited),
                                            target_inks,
                                        )?;
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
