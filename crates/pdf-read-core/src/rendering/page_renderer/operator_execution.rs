use super::*;

impl PageRenderer {
    /// Execute PDF operators to render content.
    ///
    /// OCG layer exclusion is sourced from `self.options.excluded_layers`;
    /// BDC/EMC operators referencing matching layers cause graphical operators
    /// inside that scope to be silently dropped.
    pub(super) fn execute_operators(
        &mut self,
        pixmap: &mut Pixmap,
        base_transform: Transform,
        operators: &[Operator],
        doc: &PdfDocument,
        page_num: usize,
        resources: &Object,
    ) -> Result<()> {
        // Per-render snapshot lives on `self.excluded_layers_snapshot` (filled
        // by `render_page_with_options`). Recursive calls into this function
        // reuse the same `Arc` without any allocation. We snapshot it as a
        // local `Arc::clone` (cheap pointer copy) so the operator loop below
        // can hold a `&HashSet` reference while still calling `&mut self`
        // methods through the inner match arms.
        let snapshot: Option<Arc<HashSet<String>>> = self.excluded_layers_snapshot.clone();
        static EMPTY: std::sync::OnceLock<HashSet<String>> = std::sync::OnceLock::new();
        let empty_ref: &HashSet<String> = EMPTY.get_or_init(HashSet::new);
        let excluded_layers: &HashSet<String> = snapshot.as_deref().unwrap_or(empty_ref);
        let mut gs_stack = GraphicsStateStack::new();

        // PDF default: DeviceGray, black
        {
            let gs = gs_stack.current_mut();
            gs.fill_color_space = "DeviceGray".to_string();
            gs.stroke_color_space = "DeviceGray".to_string();
            gs.fill_color_rgb = (0.0, 0.0, 0.0);
            gs.stroke_color_rgb = (0.0, 0.0, 0.0);
        }

        // Type 3 `d1` glyph description: seed the fill colour with the locked
        // current colour so the stencil paints in it. Set the same fields the
        // `rg` operator would (RGB, colour space, and components) so the
        // colour-resolution pipeline reproduces it exactly. Colour operators
        // inside the glyph are ignored below (ISO 32000-1:2008 §9.6.5.2).
        if let Some((r, g, b)) = self.type3_fill_lock {
            let gs = gs_stack.current_mut();
            gs.fill_color_rgb = (r, g, b);
            gs.fill_color_space = "DeviceRGB".to_string();
            gs.fill_color_components.clear();
            gs.fill_color_components.extend_from_slice(&[r, g, b]);
        }

        let mut in_text_object = false;
        let mut current_path = PathBuilder::new();
        let mut pending_clip: Option<(tiny_skia::Path, tiny_skia::FillRule)> = None;
        let mut clip_stack: Vec<Option<tiny_skia::Mask>> = vec![None]; // Start with no clip at depth 0

        // WS1.5b — text-clip accumulator (ISO 32000-1 §9.3.6 / Table 106,
        // `Tr` modes 4–7). Text render modes ≥4 add the union of their glyph
        // outlines to a "text clip path" that is intersected into the current
        // clip at `ET`. We accumulate that union as opaque glyph coverage in a
        // page-sized scratch pixmap's alpha channel (unioned across shows via
        // SourceOver), then convert it to a `Mask` at `ET`. This stays
        // allocation-free for the normal-text hot path: the scratch is created
        // lazily only when a mode-≥4 show actually fires, and modes 0–3 never
        // touch it. Complexity is inherently capped — coverage folds into a
        // fixed-size buffer, so no unbounded path growth is possible.
        let mut text_clip_accum: Option<Pixmap> = None;

        // OCG layer exclusion tracking.
        // `excluded_layer_depth` counts how many nested BDC/OC scopes we are
        // inside that match an excluded layer. >0 means content is suppressed.
        // `marked_content_depth` tracks total BDC/BMC nesting so EMC correctly
        // decrements only when it pops an excluded-layer entry.
        let mut excluded_layer_depth: u32 = 0;
        let mut marked_content_is_excluded: Vec<bool> = Vec::new();

        // Per-`execute_operators` resolved ExtGState resource dictionary. PDF
        // content streams often invoke `gs<N>` thousands of times per page
        // (vector scatter / contour plots emit one `gs` per marker — a
        // dense plot page can have ~10 000 such calls per Form XObject with
        // ~10 000 unique names because each marker carries its own alpha).
        // Without this hoist, every `gs` op called `doc.resolve_object(...)`
        // which deep-clones the *entire* per-form ExtGState dict (10 000+
        // entries) — that single clone dominated render time. Resolving the
        // resource dict once at the top of the operator loop and keeping a
        // borrow into it collapses the per-`gs` work to a small `get` +
        // resolve of just the inner state dict.
        let ext_g_state_resolved: Option<Object> = match resources {
            Object::Dictionary(rd) => rd.get("ExtGState").and_then(|o| doc.resolve_object(o).ok()),
            _ => None,
        };
        let ext_g_states: Option<&std::collections::HashMap<String, Object>> =
            ext_g_state_resolved.as_ref().and_then(|o| o.as_dict());
        // Cache parsed state per `dict_name` so the inner-dict resolve happens
        // at most once per unique name in scope.
        let mut ext_g_state_cache: std::collections::HashMap<String, ParsedExtGState> =
            std::collections::HashMap::new();
        for op in operators {
            // While a Type 3 `d1` glyph stencil is being painted, colour-
            // setting operators are ignored so the glyph keeps the current
            // fill colour (ISO 32000-1:2008 §9.6.5.2).
            if self.type3_fill_lock.is_some() && op.is_color_setting() {
                continue;
            }
            if self.execute_color_operator(op, &mut gs_stack, doc) {
                continue;
            }
            if self.execute_path_paint_operator(
                op,
                pixmap,
                base_transform,
                &mut current_path,
                &mut pending_clip,
                &mut clip_stack,
                &mut gs_stack,
                excluded_layer_depth,
                doc,
                page_num,
                resources,
            )? {
                continue;
            }
            if self.execute_text_show_operator(
                op,
                pixmap,
                base_transform,
                &mut gs_stack,
                &clip_stack,
                &mut text_clip_accum,
                in_text_object,
                excluded_layer_depth,
                doc,
                page_num,
                resources,
            )? {
                continue;
            }
            if self.execute_xobject_operator(
                op,
                pixmap,
                base_transform,
                &mut gs_stack,
                &clip_stack,
                excluded_layer_depth,
                doc,
                page_num,
                resources,
            )? {
                continue;
            }
            match op {
                // Graphics state operators
                Operator::SaveState => {
                    gs_stack.save();
                    // Clone current clip for the new graphics state level
                    // This allows the current level to modify its clip without affecting parents
                    let current_clip = clip_stack.last().cloned().flatten();
                    clip_stack.push(current_clip);
                    log::debug!(
                        "q (SaveState), depth={}, clip_stack depth={}",
                        gs_stack.depth(),
                        clip_stack.len()
                    );
                }
                Operator::RestoreState => {
                    gs_stack.restore();
                    // Restore previous clipping region by popping current level
                    if clip_stack.len() > 1 {
                        clip_stack.pop();
                    }
                    log::debug!(
                        "Q (RestoreState), depth={}, clip_stack depth={}",
                        gs_stack.depth(),
                        clip_stack.len()
                    );
                }
                Operator::Cm { a, b, c, d, e, f } => {
                    let matrix = Matrix {
                        a: *a,
                        b: *b,
                        c: *c,
                        d: *d,
                        e: *e,
                        f: *f,
                    };
                    let current = gs_stack.current_mut();
                    // PDF spec ISO 32000-1:2008 §8.3.4: cm concatenates as M_cm × CTM
                    current.ctm = matrix.multiply(&current.ctm);
                    log::debug!(
                        "cm: [{}, {}, {}, {}, {}, {}], CTM now: {:?}",
                        a,
                        b,
                        c,
                        d,
                        e,
                        f,
                        current.ctm
                    );
                }

                // Line style operators
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
                    // ISO 32000-1:2008 §10.7.3 `/RI` operator. Updates
                    // the graphics-state rendering-intent string; the
                    // colour stage reads `gs.rendering_intent` and
                    // dispatches qcms with the matching intent
                    // (`crate::color::RenderingIntent::from_pdf_name`
                    // maps unknown names back to /RelativeColorimetric
                    // per the spec's "unrecognised → relative" rule).
                    // Without this dispatch the parser would update
                    // the operator stream but the gs.rendering_intent
                    // field would stay at its default forever; the
                    // CMYK transform cache would collapse every
                    // intent's paint into a single shared entry.
                    gs_stack.current_mut().rendering_intent = intent.clone();
                }

                // Path construction
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
                    // Normalize negative width/height per PDF spec:
                    // re with negative dimensions means the rect extends in the opposite direction
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

                // Clipping — suppressed inside an excluded OCG scope. Per PDF
                // spec the clip is a graphics-state side-effect; without
                // gating it, a `W n` issued inside an excluded BDC scope that
                // is not bracketed by `q/Q` would silently restrict subsequent
                // visible content.
                Operator::ClipNonZero => {
                    if excluded_layer_depth == 0 {
                        if let Some(path) = current_path.clone().finish() {
                            pending_clip = Some((path, tiny_skia::FillRule::Winding));
                        }
                    }
                }
                Operator::ClipEvenOdd => {
                    if excluded_layer_depth == 0 {
                        if let Some(path) = current_path.clone().finish() {
                            pending_clip = Some((path, tiny_skia::FillRule::EvenOdd));
                        }
                    }
                }

                // Text object operators
                Operator::BeginText => {
                    in_text_object = true;
                    // Start each text object with a clean text-clip path
                    // (§9.4.1: the text clip path is reset at BT and applied
                    // at ET). Any leftover from a malformed/unterminated prior
                    // block is discarded here.
                    text_clip_accum = None;
                    let gs = gs_stack.current_mut();
                    gs.text_matrix = Matrix::identity();
                    gs.text_line_matrix = Matrix::identity();
                    log::debug!("BT (BeginText)");
                }
                Operator::EndText => {
                    in_text_object = false;
                    // WS1.5b — apply the accumulated text clip path (Tr 4–7).
                    // If no clip-mode text was shown the accumulator is None and
                    // ET behaves exactly as before. An all-transparent
                    // accumulator (e.g. every glyph was whitespace or lacked an
                    // outline) is treated as degenerate and leaves the clip
                    // unchanged rather than collapsing it to empty.
                    if let Some(scratch) = text_clip_accum.take() {
                        let has_coverage = scratch.data().chunks_exact(4).any(|px| px[3] != 0);
                        if has_coverage {
                            let text_mask = tiny_skia::Mask::from_pixmap(
                                scratch.as_ref(),
                                tiny_skia::MaskType::Alpha,
                            );
                            // Intersect (logical AND) the glyph silhouette with
                            // the current scope's clip so subsequent content is
                            // confined to the text shape *within* the existing
                            // clip — never widened past it.
                            if let Some(slot) = clip_stack.last_mut() {
                                let existing = slot.take();
                                *slot =
                                    Some(intersect_with_inherited(text_mask, existing.as_ref()));
                            }
                        }
                    }
                }

                // Text state operators
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

                // Text positioning
                Operator::Td { tx, ty } => {
                    if in_text_object {
                        let gs = gs_stack.current_mut();
                        let translation = Matrix::translation(*tx, *ty);
                        gs.text_line_matrix = translation.multiply(&gs.text_line_matrix);
                        gs.text_matrix = gs.text_line_matrix;
                        log::debug!(
                            "Td: [{}, {}], text_matrix now: {:?}",
                            tx,
                            ty,
                            gs.text_matrix
                        );
                    }
                }
                Operator::TD { tx, ty } => {
                    if in_text_object {
                        let gs = gs_stack.current_mut();
                        gs.leading = -(*ty);
                        let translation = Matrix::translation(*tx, *ty);
                        gs.text_line_matrix = translation.multiply(&gs.text_line_matrix);
                        gs.text_matrix = gs.text_line_matrix;
                        log::debug!(
                            "TD: [{}, {}], text_matrix now: {:?}",
                            tx,
                            ty,
                            gs.text_matrix
                        );
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
                        log::debug!(
                            "Tm: [{}, {}, {}, {}, {}, {}], text_matrix now: {:?}",
                            a,
                            b,
                            c,
                            d,
                            e,
                            f,
                            gs.text_matrix
                        );
                    }
                }
                Operator::TStar => {
                    if in_text_object {
                        let gs = gs_stack.current_mut();
                        let leading = gs.leading;
                        let translation = Matrix::translation(0.0, -leading);
                        gs.text_line_matrix = translation.multiply(&gs.text_line_matrix);
                        gs.text_matrix = gs.text_line_matrix;
                        log::debug!("T*: text_matrix now: {:?}", gs.text_matrix);
                    }
                }
                Operator::Tf { font, size } => {
                    // Cache the font's writing mode on the graphics state so
                    // the rasterizer hot path can branch on a single
                    // primitive read instead of dereferencing the FontInfo
                    // through the cache for every glyph.
                    let wmode = self.fonts.get(font).map(|f| f.wmode).unwrap_or(0);
                    let gs = gs_stack.current_mut();
                    gs.font_name = Some(font.clone());
                    gs.font_size = *size;
                    gs.text_wmode = wmode;
                }

                // Extended graphics state
                Operator::SetExtGState { dict_name } => {
                    // Fast path: resource dict is already resolved (see top of
                    // this function), so the per-`gs` cost is one HashMap
                    // lookup + one resolve of the small inner state dict.
                    let entry = ext_g_state_cache
                        .entry(dict_name.clone())
                        .or_insert_with(|| {
                            if let Some(states) = ext_g_states {
                                if let Some(state_obj) = states.get(dict_name) {
                                    return parse_ext_g_state_inner(state_obj, doc)
                                        .unwrap_or_default();
                                }
                            }
                            ParsedExtGState::default()
                        });
                    entry.apply(gs_stack.current_mut());
                }

                // EndPath (n operator): discard current path without painting,
                // but apply any pending clip. Per PDF spec, W n is the standard
                // way to set a clipping path without filling or stroking.
                // Suppress the clip application inside an excluded OCG scope so
                // the clip doesn't leak past EMC into visible content.
                Operator::EndPath => {
                    if excluded_layer_depth == 0 {
                        apply_pending_clip(
                            &mut pending_clip,
                            &mut clip_stack,
                            pixmap,
                            base_transform,
                            &gs_stack,
                        );
                    } else {
                        // Drop any pending clip without applying it.
                        let _ = pending_clip.take();
                    }
                    current_path = PathBuilder::new();
                }

                // Shading (gradient) operator — suppressed when inside excluded layer
                Operator::PaintShading { name } => {
                    if excluded_layer_depth == 0 {
                        let mut gs_clone = gs_stack.current().clone();
                        // §8.7.4 + §11.7.3: when the shading's
                        // /ColorSpace is /Separation or non-process
                        // /DeviceN, surface the ink-name list (paired
                        // with the /Function /C0 endpoint tints) onto
                        // `gs_clone.fill_spot_inks` so the spot mirror
                        // sees a non-empty source ink set and fires.
                        // Without this the shading paint silently
                        // bypasses the spot mirror because the gating
                        // (`spot_paint_active`) checks
                        // `gs.fill_spot_inks`, which is otherwise
                        // populated only by `cs`/`scn` colour-set
                        // operators — none of which fire before `sh`.
                        if !self.spot_paint_active(&gs_clone, true) && self.cmyk_sidecar.is_some() {
                            if let Some(inks) = self.resolve_shading_spot_inks(name, resources, doc)
                            {
                                if !inks.is_empty() {
                                    gs_clone.fill_spot_inks = inks;
                                }
                            }
                        }
                        let transform = combine_transforms(base_transform, &gs_clone.ctm);
                        let clip = clip_stack.last().and_then(|c| c.as_ref());
                        // §11.4.7 + §11.7.4 + §11.4 cycle: shading is
                        // a fill-side paint, so the snapshot/apply
                        // cadence mirrors the path-Fill arm. The
                        // overprint and compose-first paths short-
                        // circuit when the active fill colour is not
                        // CMYK (the shading paint's per-pixel colour
                        // comes from the gradient interpolator, not
                        // `gs.fill_color_cmyk`), so they only fire when
                        // the page set a CMYK fill before invoking
                        // `sh`.
                        let smask_snap = self.smask_snapshot(pixmap, &gs_clone);
                        let smask_spot_snap = self.smask_spot_snapshot(&gs_clone);
                        let overprint_snap = self.overprint_snapshot(pixmap, &gs_clone, true);
                        let cmyk_compose_snap =
                            self.cmyk_compose_snapshot(pixmap, &gs_clone, doc, true);
                        let spot_snap = self.spot_paint_snapshot(pixmap, &gs_clone, true);
                        // §8.7.4 + §11.7.3: rasterise the shading
                        // geometry (intersected with the active clip)
                        // so the spot mirror sees the geometry-true
                        // per-pixel coverage of the gradient.
                        let shading_coverage = spot_snap.as_ref().and_then(|_| {
                            self.rasterise_shading_coverage(
                                name, transform, &gs_clone, resources, doc, clip,
                            )
                        });
                        self.render_shading(
                            pixmap, name, transform, &gs_clone, resources, doc, clip,
                        )?;
                        if let Some(snap) = cmyk_compose_snap {
                            self.apply_cmyk_compose_after_paint(
                                pixmap, &snap, &gs_clone, doc, true,
                            );
                        }
                        if let Some(snap) = overprint_snap {
                            self.apply_overprint_after_paint(pixmap, &snap, &gs_clone, doc, true);
                        }
                        if let Some(snap) = spot_snap {
                            self.mirror_spot_paint_into_sidecar_with_coverage(
                                pixmap,
                                &snap,
                                shading_coverage.as_deref(),
                                &gs_clone,
                                true,
                            );
                        }
                        if let Some(snap) = smask_snap {
                            self.apply_smask_after_paint(
                                pixmap,
                                &snap,
                                smask_spot_snap.as_deref(),
                                &gs_clone,
                                doc,
                                page_num,
                                resources,
                                base_transform,
                            )?;
                        }
                    }
                }

                // Marked content operators — track OCG layer exclusion
                Operator::BeginMarkedContent { .. } => {
                    marked_content_is_excluded.push(false);
                }
                Operator::BeginMarkedContentDict { tag, properties } => {
                    let mut is_excluded = false;
                    // Tag "OC" scopes can hide content even with empty excluded_layers
                    // when the OCMD uses /VE /Not or /P /AllOff/AnyOff (the
                    // expression evaluates with all OCGs on by default). We can
                    // only short-circuit cheaply for simple OCG refs, which the
                    // optional_content module handles internally.
                    if tag == "OC" {
                        is_excluded = crate::optional_content::resolve_and_check_ocg_excluded(
                            properties,
                            Some(resources),
                            Some(doc),
                            excluded_layers,
                        );
                    }
                    if is_excluded {
                        excluded_layer_depth += 1;
                    }
                    marked_content_is_excluded.push(is_excluded);
                }
                Operator::EndMarkedContent => {
                    if let Some(was_excluded) = marked_content_is_excluded.pop() {
                        if was_excluded && excluded_layer_depth > 0 {
                            excluded_layer_depth -= 1;
                        }
                    }
                }

                _ => {}
            }
        }

        Ok(())
    }
}
