use super::*;

/// Render text into every target separation pixmap, routing each glyph
/// through the per-ink tint. The strategy is to clone the GraphicsState,
/// replace its fill colour with a grayscale paint equal to the tint, and
/// reuse the standard [`TextRasterizer`]. This preserves glyph shape,
/// kerning, and anti-aliasing — the same fidelity as the page renderer.
///
/// The returned advance is shared across all plates (the rasteriser is
/// deterministic for a given font/text/state, so each plate's advance
/// agrees) — we use the last computed value, matching the single-plate
/// behaviour. If no plate is touched (every plate's [`PaintAction`] is
/// `Skip`, or render mode 3) the advance is computed from the font
/// metrics so the text matrix still progresses correctly.
#[allow(clippy::too_many_arguments)]
pub(super) fn render_text_to_plate(
    pixmaps: &mut [Pixmap],
    text: &[u8],
    base_transform: Transform,
    gs_stack: &mut GraphicsStateStack,
    color_state_stack: &[SeparationColorState],
    color_spaces: &HashMap<String, Object>,
    resources: &Object,
    ctx: &mut SeparationContext<'_>,
    clip: Option<&Mask>,
    target_inks: &[&str],
) -> Result<f32> {
    let gs = gs_stack.current();
    let empty = SeparationColorState::new();
    let cs = color_state_stack.last().unwrap_or(&empty);

    // Render mode 3 = invisible text; mode 7 = clip-only, no paint (§9.3.6,
    // WS1.5). Both advance the text matrix but skip painting.
    if gs.render_mode == 3 || gs.render_mode == 7 {
        return measure_text_advance(text, gs, ctx.fonts);
    }

    let transform = combine_transforms(base_transform, &gs.ctm);
    let mut painted_advance: Option<f32> = None;

    for (i, &ink) in target_inks.iter().enumerate() {
        let tint = match tint_for_ink(
            true,
            gs,
            color_spaces,
            resources,
            ctx.doc,
            ink,
            &cs.fill_components,
            &cs.stroke_components,
        ) {
            PaintAction::Paint(t) => t,
            PaintAction::Skip => continue,
        };

        // Build a faked-grayscale GraphicsState so the rasteriser paints in
        // (tint, tint, tint) which becomes the plate value in the R channel.
        let mut faux = gs.clone();
        faux.fill_color_rgb = (tint, tint, tint);
        faux.fill_alpha = 1.0;
        faux.blend_mode = "Normal".to_string();

        let advance = ctx.text_rasterizer.render_text(
            &mut pixmaps[i],
            text,
            transform,
            &faux,
            // The separation backend bakes its own faux grayscale into
            // `faux.fill_color_rgb`; the composite-side resolution pipeline
            // is not in play here, so no colour override is needed.
            None,
            resources,
            ctx.doc,
            clip,
            ctx.fonts,
        )?;
        painted_advance = Some(advance);
    }

    match painted_advance {
        Some(a) => Ok(a),
        // No plate was touched by this text — still advance the matrix so
        // subsequent glyphs land at the correct position.
        None => measure_text_advance(text, gs, ctx.fonts),
    }
}

/// Render a TJ array (sequence of strings + offsets) into all target
/// plates. Walks the array applying offsets between strings, painting
/// each string component via [`render_text_to_plate`].
#[allow(clippy::too_many_arguments)]
pub(super) fn render_tj_to_plate(
    pixmaps: &mut [Pixmap],
    array: &[TextElement],
    base_transform: Transform,
    gs_stack: &mut GraphicsStateStack,
    color_state_stack: &[SeparationColorState],
    color_spaces: &HashMap<String, Object>,
    resources: &Object,
    ctx: &mut SeparationContext<'_>,
    clip: Option<&Mask>,
    target_inks: &[&str],
) -> Result<f32> {
    let mut total_advance = 0.0;
    for element in array {
        match element {
            TextElement::String(text) => {
                let advance = render_text_to_plate(
                    pixmaps,
                    text,
                    base_transform,
                    gs_stack,
                    color_state_stack,
                    color_spaces,
                    resources,
                    ctx,
                    clip,
                    target_inks,
                )?;
                gs_stack.current_mut().advance_text_matrix(advance);
                total_advance += advance;
            }
            TextElement::Offset(offset) => {
                let shift = (-*offset / 1000.0) * gs_stack.current().font_size;
                gs_stack.current_mut().advance_text_matrix(shift);
                total_advance += shift;
            }
        }
    }
    Ok(total_advance)
}

/// Compute the horizontal advance a [`TextRasterizer`] call would
/// produce, without painting. Used for invisible/skipped text so the
/// text matrix stays consistent with the painted ink plates.
///
/// Best-effort: when an embedded width table is unavailable we fall
/// back to `font_size * len * 0.5` — close enough to keep glyph
/// positions inside the rest of the line.
pub(super) fn measure_text_advance(
    text: &[u8],
    gs: &GraphicsState,
    fonts: &HashMap<String, Arc<FontInfo>>,
) -> Result<f32> {
    let font_info = gs
        .font_name
        .as_ref()
        .and_then(|n| fonts.get(n))
        .map(Arc::clone);

    // Sum widths from the font's width table (in glyph units / 1000)
    // multiplied by font_size, plus per-char Tc spacing.
    let mut units: f32 = 0.0;
    let mut count: usize = 0;
    if let Some(info) = font_info.as_ref() {
        if info.subtype != "Type0" {
            for &b in text {
                units += info.get_glyph_width(b as u16);
                count += 1;
            }
        } else {
            // Type0: iterate 2-byte codes (approx).
            let mut i = 0;
            while i + 1 < text.len() {
                let code = ((text[i] as u16) << 8) | text[i + 1] as u16;
                units += info.get_glyph_width(code);
                count += 1;
                i += 2;
            }
        }
    } else {
        for _ in text {
            units += 500.0;
            count += 1;
        }
    }
    let advance = units * gs.font_size / 1000.0 + (count as f32) * gs.char_space;
    Ok(advance)
}

/// Fill a path into the separation pixmap with the given tint value.
///
/// `pub(crate)` so the resolution pipeline's [`super::resolution::SeparationBackend`]
/// can take it as a parity reference in its byte-for-byte equivalence test.
/// The shipping per-plate walker calls it directly; production callers
/// outside the renderer should not.
pub(crate) fn fill_separation(
    pixmap: &mut Pixmap,
    path: &tiny_skia::Path,
    transform: Transform,
    tint: f32,
    fill_rule: FillRule,
    clip: Option<&Mask>,
) {
    let gray = (tint.clamp(0.0, 1.0) * 255.0).round() as u8;
    let color = tiny_skia::Color::from_rgba8(gray, gray, gray, 255);
    let mut paint = tiny_skia::Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;
    // SourceOver with opaque (alpha=255) source = replacement; this matches
    // PDF's opaque painting model where each new fill overwrites the pixels
    // under it within the path. Overlapping fills are *not* accumulated —
    // PDF separation semantics dictate last-writer-wins per ink at the
    // overlapping pixels, which SourceOver gives us for free.
    paint.blend_mode = tiny_skia::BlendMode::SourceOver;

    pixmap.fill_path(path, &paint, fill_rule, transform, clip);
}
