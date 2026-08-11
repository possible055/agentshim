use super::*;

/// Resize an RGBA (straight-alpha) byte buffer using SIMD-accelerated bilinear filtering.
///
/// Returns `None` on failure (zero dimensions, SIMD dispatch error) so callers
/// can fall back to tiny_skia's own resampling path.
pub(super) fn resize_rgba(
    src: &[u8],
    src_w: u32,
    src_h: u32,
    dst_w: u32,
    dst_h: u32,
) -> Option<Vec<u8>> {
    use fast_image_resize::images::Image;
    use fast_image_resize::pixels::PixelType;
    use fast_image_resize::{FilterType, ResizeAlg, ResizeOptions, Resizer};

    // from_slice_u8 needs a mutable slice; copy into a local buffer.
    let mut buf = src.to_vec();
    let src_img = Image::from_slice_u8(src_w, src_h, &mut buf, PixelType::U8x4).ok()?;
    let mut dst_img = Image::new(dst_w, dst_h, PixelType::U8x4);
    Resizer::new()
        .resize(
            &src_img,
            &mut dst_img,
            &ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Bilinear)),
        )
        .ok()?;
    Some(dst_img.into_vec())
}

/// Encode a tiny_skia `Pixmap` to PNG.
///
/// Uses fdeflate (ultra-fast) compression via the `image` crate instead of
/// tiny_skia's built-in `encode_png`, which defaults to flate2 level 6 and is
/// 3–5× slower on typical page images.
pub(super) fn encode_png(pixmap: &Pixmap) -> Result<Vec<u8>> {
    let w = pixmap.width();
    let h = pixmap.height();

    // Demultiply: tiny_skia stores premultiplied RGBA; PNG expects straight alpha.
    let src = pixmap.data();
    let mut data = src.to_vec();
    for chunk in data.chunks_exact_mut(4) {
        let a = chunk[3];
        if a != 0 && a != 255 {
            let a32 = a as u32;
            chunk[0] = ((chunk[0] as u32 * 255 + a32 / 2) / a32).min(255) as u8;
            chunk[1] = ((chunk[1] as u32 * 255 + a32 / 2) / a32).min(255) as u8;
            chunk[2] = ((chunk[2] as u32 * 255 + a32 / 2) / a32).min(255) as u8;
        }
    }

    use image::codecs::png::{CompressionType, FilterType, PngEncoder};
    use image::ImageEncoder;
    let mut output = Vec::new();
    PngEncoder::new_with_quality(&mut output, CompressionType::Fast, FilterType::Sub)
        .write_image(&data, w, h, image::ExtendedColorType::Rgba8)
        .map_err(|e| Error::InvalidPdf(format!("PNG encoding failed: {}", e)))?;
    Ok(output)
}

/// Combine two transformations.
pub(super) fn combine_transforms(base: Transform, ctm: &Matrix) -> Transform {
    base.pre_concat(Transform::from_row(
        ctm.a, ctm.b, ctm.c, ctm.d, ctm.e, ctm.f,
    ))
}

/// Parse a Type 3 font's `/FontMatrix` into a glyph-space → text-space
/// transform. Defaults to the Type 1 matrix `[0.001 0 0 0.001 0 0]` when the
/// entry is missing or malformed (ISO 32000-1 §9.6.5).
pub(super) fn type3_font_matrix(font_dict: &HashMap<String, Object>) -> Transform {
    if let Some(arr) = font_dict.get("FontMatrix").and_then(|o| o.as_array()) {
        if arr.len() == 6 {
            let f = |i: usize| -> Option<f32> {
                arr[i]
                    .as_real()
                    .map(|r| r as f32)
                    .or_else(|| arr[i].as_integer().map(|v| v as f32))
            };
            if let (Some(a), Some(b), Some(c), Some(d), Some(e), Some(g)) =
                (f(0), f(1), f(2), f(3), f(4), f(5))
            {
                if [a, b, c, d, e, g].iter().all(|v| v.is_finite()) {
                    return Transform::from_row(a, b, c, d, e, g);
                }
            }
        }
    }
    Transform::from_row(0.001, 0.0, 0.0, 0.001, 0.0, 0.0)
}

/// Decode a `/CharProcs` glyph stream. Resolves an indirect reference (with
/// encryption support) or decodes a direct stream. Returns `None` for a
/// non-stream object or on decode failure, so the caller skips the glyph.
pub(super) fn decode_type3_charproc(doc: &PdfDocument, obj: &Object) -> Option<Vec<u8>> {
    if let Some(obj_ref) = obj.as_reference() {
        let resolved = doc.load_object(obj_ref).ok()?;
        if matches!(resolved, Object::Stream { .. }) {
            return doc.decode_stream_with_encryption(&resolved, obj_ref).ok();
        }
        return None;
    }
    if matches!(obj, Object::Stream { .. }) {
        return obj.decode_stream_data().ok();
    }
    None
}

/// Inclusive tile-index range `[lo, hi]` (along one axis) whose cells
/// intersect the device interval `[region_lo, region_hi]`.
///
/// Tile `i` occupies device coordinates `[cell_min + i·step,
/// cell_min + i·step + cell_extent]`. Solving for the indices whose cell
/// interval overlaps the region gives the two bounds; `step` may be
/// negative (a flipped pattern matrix), so the candidates are ordered by
/// `min`/`max` rather than assuming a sign. The range is deliberately
/// over-inclusive by up to one tile on each side (the per-tile clip mask
/// discards any cell that falls entirely outside the fill path), which
/// keeps the arithmetic branch-free.
///
/// `step` must be non-zero — callers guard `|step| >= 0.5` device px
/// before calling — so this never divides by zero.
pub(super) fn axis_tile_range(
    region_lo: f32,
    region_hi: f32,
    cell_min: f32,
    cell_extent: f32,
    step: f32,
) -> (i32, i32) {
    let a = (region_lo - cell_extent - cell_min) / step;
    let b = (region_hi - cell_min) / step;
    let lo = a.min(b).floor();
    let hi = a.max(b).ceil();
    // Clamp to i32 so an absurd (but guard-passing) region cannot overflow;
    // the caller additionally caps the total tile count.
    (
        lo.max(i32::MIN as f32) as i32,
        hi.min(i32::MAX as f32) as i32,
    )
}

/// Build the image-space → user-space transform for a PDF image blit.
///
/// Per ISO 32000-1 §8.9.5, PDF images live in a unit square in the user
/// coordinate system; image rows are top-to-bottom (opposite of PDF's
/// bottom-to-top y axis). The pre-translate-by-1-in-y + pre-scale-by
/// `1/src_w, -1/src_h` flips the rows AND normalises the source-pixel
/// extent to the unit square, so the caller's `parent` CTM places the
/// image where the PDF demands.
///
/// Shared by `render_image` and `render_image_mask`.
pub(super) fn image_unit_square_transform(parent: Transform, src_w: u32, src_h: u32) -> Transform {
    parent
        .pre_translate(0.0, 1.0)
        .pre_scale(1.0 / src_w as f32, -1.0 / src_h as f32)
}

/// Build the `PixmapPaint` used to blit an already-flipped image into
/// the page pixmap.
///
/// `image_transform` must already be the output of
/// [`image_unit_square_transform`] (or the SIMD fast path's
/// translate-only equivalent); the helper reads its scale to pick
/// Bicubic when the blit is an upscale or 1:1 and Bilinear when it is a
/// downscale — the same heuristic both `render_image` and
/// `render_image_mask` used independently before this consolidation.
/// `opacity` is the source's alpha (the std-image path passes
/// `gs.fill_alpha`; the ImageMask path bakes alpha into the stencil
/// pixels and passes `1.0`). `blend_mode_pdf` is the PDF blend-mode
/// name from `gs.blend_mode`.
///
/// Shared by `render_image` and `render_image_mask`.
pub(super) fn pixmap_paint_for_image_blit(
    image_transform: Transform,
    opacity: f32,
    blend_mode_pdf: &str,
) -> PixmapPaint {
    let mut paint = PixmapPaint::default();
    paint.opacity = opacity;
    paint.blend_mode = crate::rendering::pdf_blend_mode_to_skia(blend_mode_pdf);
    let (xs, ys) = image_transform.get_scale();
    paint.quality = if xs >= 1.0 || ys >= 1.0 {
        tiny_skia::FilterQuality::Bicubic
    } else {
        tiny_skia::FilterQuality::Bilinear
    };
    paint
}

/// Convert DeviceCMYK (0.0-1.0) to DeviceRGB (0.0-1.0) using the PROCESS-INK
/// conversion (`crate::color::cmyk_to_rgb`, tetralinear over the 16 measured ink
/// corners), NOT the naive additive-clamp `R = 1 - min(1, C+K)`. This unifies
/// the renderer's DeviceCMYK display with the text/extraction and image paths so
/// the same CMYK value resolves to the same RGB everywhere (100% K is `#231F20`,
/// 100% cyan `#00ADEF`). The RGB->CMYK sidecar inverse is
/// `crate::color::rgb_to_cmyk`, which keeps the overprint round-trip consistent
/// within the process gamut. A real ICC/OutputIntent CMM still takes precedence
/// when a profile is available.
pub(super) fn cmyk_to_rgb(c: f32, m: f32, y: f32, k: f32) -> (f32, f32, f32) {
    crate::color::cmyk_to_rgb(c, m, y, k)
}

/// Parse a colour-key `/Mask` array (ISO 32000-1 §8.9.6.4) into per-component
/// `(min, max)` sample ranges. The array is `[min1 max1 min2 max2 ...]` with one
/// pair per colour component, in the image's pre-Decode component space.
///
/// Returns `None` for any malformed array — wrong length for `ncomp`, a
/// non-integer entry, a negative bound, or `min > max` — so the caller can fall
/// back to no masking rather than guess.
pub(super) fn parse_color_key_mask(arr: &[Object], ncomp: usize) -> Option<Vec<(u32, u32)>> {
    if ncomp == 0 || arr.len() != ncomp * 2 {
        return None;
    }
    let mut ranges = Vec::with_capacity(ncomp);
    for pair in arr.chunks_exact(2) {
        let lo = pair[0].as_integer()?;
        let hi = pair[1].as_integer()?;
        if lo < 0 || hi < 0 || lo > hi {
            return None;
        }
        ranges.push((lo as u32, hi as u32));
    }
    Some(ranges)
}

/// Returns `true` when a pixel's raw component samples all fall within their
/// corresponding colour-key `(min, max)` ranges, meaning the pixel must be made
/// fully transparent (ISO 32000-1 §8.9.6.4). Returns `false` on any length
/// mismatch so a bad range set never masks.
pub(super) fn color_key_pixel_masked(components: &[u8], ranges: &[(u32, u32)]) -> bool {
    if components.is_empty() || components.len() != ranges.len() {
        return false;
    }
    components
        .iter()
        .zip(ranges.iter())
        .all(|(&c, &(lo, hi))| (c as u32) >= lo && (c as u32) <= hi)
}

/// Apply a colour-key `/Mask` to an already-decoded RGBA image by zeroing the
/// alpha of every source pixel whose raw component samples all fall within the
/// mask ranges.
///
/// Colour-key masking is defined against the raw pre-Decode samples. Those are
/// only recoverable from an 8-bit `ImageData::Raw` buffer whose per-pixel byte
/// count matches `ranges.len()`. For anything else (JPEG, non-8-bit depths, or a
/// palette-expanded Indexed image whose original indices are lost) the ranges
/// cannot be mapped onto the decoded pixels, so masking is skipped rather than
/// applied incorrectly.
pub(super) fn apply_color_key_mask(
    image: &crate::extractors::images::PdfImage,
    ranges: &[(u32, u32)],
    rgba: &mut image::RgbaImage,
) {
    use crate::extractors::images::ImageData;

    let ncomp = ranges.len();
    let ImageData::Raw { pixels, format } = image.data() else {
        log::debug!("color-key /Mask: non-raw (e.g. JPEG) image, skipping");
        return;
    };
    if image.bits_per_component() != 8 || format.bytes_per_pixel() != ncomp {
        log::debug!(
            "color-key /Mask: unsupported layout (bpc={}, bpp={}, ncomp={}), skipping",
            image.bits_per_component(),
            format.bytes_per_pixel(),
            ncomp
        );
        return;
    }
    let w = rgba.width() as usize;
    let h = rgba.height() as usize;
    if pixels.len() < w * h * ncomp {
        log::debug!("color-key /Mask: sample buffer too small, skipping");
        return;
    }
    for y in 0..h {
        for x in 0..w {
            let base = (y * w + x) * ncomp;
            if color_key_pixel_masked(&pixels[base..base + ncomp], ranges) {
                rgba.get_pixel_mut(x as u32, y as u32)[3] = 0;
            }
        }
    }
}
