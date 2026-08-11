use super::*;

/// Resolve the image XObject's declared `/ColorSpace` to a [`ResolvedSpace`].
///
/// Handles all three syntactic shapes the spec allows:
/// - `/DeviceCMYK` (a name) — direct, honouring `Default*` remap from the
///   page's `/Resources/ColorSpace` per §8.6.5.6.
/// - `[/Separation /InkName /Alt /Tint]` (inline array) — classified directly.
/// - An indirect reference to either of the above — resolved first.
pub(super) fn resolve_image_color_space(
    image_dict: &HashMap<String, Object>,
    color_spaces: &HashMap<String, Object>,
    resources: &Object,
    doc: &PdfDocument,
) -> ResolvedSpace {
    let cs_obj = match image_dict.get("ColorSpace") {
        Some(o) => o,
        None => return ResolvedSpace::Unknown,
    };
    let resolved_obj = match cs_obj.as_reference() {
        Some(r) => match doc.load_object(r) {
            Ok(o) => o,
            Err(_) => return ResolvedSpace::Unknown,
        },
        None => cs_obj.clone(),
    };
    if let Some(name) = resolved_obj.as_name() {
        return resolve_color_space(name, color_spaces, resources, doc);
    }
    classify_resolved(&resolved_obj, color_spaces, resources, doc)
}

/// For a given source colour space and target ink, return the index of the
/// channel that contributes to that ink, or `None` when the ink is outside
/// the source's colorant set.
///
/// Matching mirrors `tint_for_ink`:
/// - DeviceCMYK / IccCmyk: Cyan/Magenta/Yellow/Black → 0/1/2/3, spots → None
/// - Separation(name): match against the named ink (and `/All`); else None
/// - DeviceN(names): position of the matching name (or `/All`)
/// - RGB / Gray / Unknown: None (no plate intent)
pub(super) fn image_channel_for_ink(space: &ResolvedSpace, ink: &str) -> Option<usize> {
    match space {
        ResolvedSpace::Cmyk | ResolvedSpace::IccCmyk => match ink {
            "Cyan" => Some(0),
            "Magenta" => Some(1),
            "Yellow" => Some(2),
            "Black" => Some(3),
            _ => None,
        },
        ResolvedSpace::Separation(name) => {
            if name == "None" {
                None
            } else if name == "All" || name == ink {
                Some(0)
            } else {
                None
            }
        }
        ResolvedSpace::DeviceN(names) => names
            .iter()
            .position(|n| n.as_str() != "None" && (n == "All" || n == ink)),
        ResolvedSpace::Rgb
        | ResolvedSpace::Gray
        | ResolvedSpace::IccRgb
        | ResolvedSpace::IccGray
        | ResolvedSpace::Unknown => None,
    }
}

/// Pull samples for `target_channel` out of an interleaved 8-bpc image buffer
/// (`stride` bytes per pixel). Returns a `W*H` byte plane suitable for blitting
/// as the R channel of an opaque grayscale RGBA pixmap.
pub(super) fn extract_image_channel(
    samples: &[u8],
    pixel_count: usize,
    stride: usize,
    target_channel: usize,
) -> Vec<u8> {
    let mut plane = Vec::with_capacity(pixel_count);
    for p in 0..pixel_count {
        let off = p * stride + target_channel;
        plane.push(samples.get(off).copied().unwrap_or(0));
    }
    plane
}

/// Blit a single-channel ink-coverage plane (`W*H` bytes, 0 = no ink, 255 =
/// full ink) into the destination separation pixmap at the image's
/// CTM-derived transform. The image is treated as occupying the PDF unit
/// square in user space; the formula mirrors `page_renderer.rs:2146-2148`
/// (pre-translate y by 1, pre-scale 1/w by -1/h to flip the row order).
pub(super) fn blit_image_plane_to_plate(
    dst: &mut Pixmap,
    plane: &[u8],
    src_w: u32,
    src_h: u32,
    transform: Transform,
    clip: Option<&Mask>,
) {
    // Build an opaque RGBA buffer where every channel carries the plane
    // value. `Pixmap::draw_pixmap` then composites with SourceOver at
    // alpha 255 (opaque replacement), so the R channel at the destination
    // ends up equal to the source value — the same convention as
    // `fill_separation`.
    let n = (src_w as usize) * (src_h as usize);
    if plane.len() < n {
        return;
    }
    let mut rgba = Vec::with_capacity(n * 4);
    for &v in &plane[..n] {
        rgba.extend_from_slice(&[v, v, v, 255]);
    }
    let Some(size) = tiny_skia::IntSize::from_wh(src_w, src_h) else {
        return;
    };
    let Some(src) = Pixmap::from_vec(rgba, size) else {
        return;
    };
    let image_transform = transform
        .pre_translate(0.0, 1.0)
        .pre_scale(1.0 / src_w as f32, -1.0 / src_h as f32);
    let mut paint = tiny_skia::PixmapPaint::default();
    paint.blend_mode = tiny_skia::BlendMode::SourceOver;
    paint.quality = tiny_skia::FilterQuality::Bilinear;
    dst.draw_pixmap(0, 0, src.as_ref(), &paint, image_transform, clip);
}

/// Returns true if the image XObject's `/Filter` chain contains a filter we
/// can't decode. `/JPXDecode` (JPEG 2000) is decodable only when the `jpeg2000`
/// feature is built in; without it, JPX images are still skipped here.
pub(super) fn image_has_unsupported_filter(image_dict: &HashMap<String, Object>) -> bool {
    let filter = match image_dict.get("Filter") {
        Some(f) => f,
        None => return false,
    };
    let names: Vec<&str> = match filter {
        Object::Name(n) => vec![n.as_str()],
        Object::Array(arr) => arr.iter().filter_map(|o| o.as_name()).collect(),
        _ => vec![],
    };
    #[cfg(feature = "jpeg2000")]
    {
        let _ = &names;
        false
    }
    #[cfg(not(feature = "jpeg2000"))]
    {
        names.iter().any(|f| matches!(*f, "JPXDecode" | "J2"))
    }
}

/// Paint an image XObject into the separation plates.
///
/// Per ISO 32000-1 §11.7.4 image samples are routed channel-by-channel to
/// the matching ink plates. §11.7.4.3 explicitly carves images out of the
/// `OPM` rule — this function never consults `gs.overprint_mode`.
///
/// Currently in scope:
/// - DeviceCMYK / ICCBased(N=4) images → C/M/Y/K plates
/// - Separation images → the named spot plate
/// - DeviceN images → per-channel routing by colorant name
/// - Image masks (`/ImageMask true`) → paint the current fill colour through
///   the 1-bit stencil (delegates to `tint_for_ink` for spot/process logic)
/// - JPX-filtered images logged and skipped (no decoder bundled)
///
/// Out of scope, dropped silently for now: RGB/Gray images, indexed images,
/// inline images. See module-level Limitations.
#[allow(clippy::too_many_arguments)]
pub(super) fn paint_image_to_plates(
    pixmaps: &mut [Pixmap],
    name: &str,
    xobject: &Object,
    obj_ref: Option<crate::object::ObjectRef>,
    base_transform: Transform,
    gs_stack: &GraphicsStateStack,
    color_state: Option<&SeparationColorState>,
    color_spaces: &HashMap<String, Object>,
    resources: &Object,
    ctx: &SeparationContext<'_>,
    clip: Option<&Mask>,
    target_inks: &[&str],
) -> Result<()> {
    use crate::extractors::images::{
        extract_image_from_xobject, ColorSpace as PdfCs, ImageData, PixelFormat,
    };

    let dict = match xobject {
        Object::Stream { dict, .. } => dict,
        _ => return Ok(()),
    };

    // §8.9.6.2: image masks are 1-bpc stencils painted with the current
    // non-stroking colour, not channel-bearing images. Route through the
    // same per-plate logic as a vector fill.
    let is_image_mask = dict
        .get("ImageMask")
        .map(|o| matches!(o, Object::Boolean(true)))
        .unwrap_or(false);
    if is_image_mask {
        return paint_image_mask_to_plates(
            pixmaps,
            name,
            xobject,
            obj_ref,
            base_transform,
            gs_stack,
            color_state,
            color_spaces,
            resources,
            ctx,
            clip,
            target_inks,
        );
    }

    // §D3: JPX images get a debug log and are dropped — no pure-Rust JP2
    // decoder is bundled.
    if image_has_unsupported_filter(dict) {
        log::warn!(
            "Skipping image XObject '{name}' on separation plates: \
             unsupported filter (JPXDecode — JPEG 2000 decoder not bundled)"
        );
        return Ok(());
    }

    // Resolve the image's declared colour space, honouring DefaultCMYK etc.
    let resolved_space = resolve_image_color_space(dict, color_spaces, resources, ctx.doc);

    // For RGB / Gray / Unknown the image carries no ink-coverage intent.
    // Skip entirely; underlying plates are left untouched. pdf_oxide does
    // not synthesise CMYK from RGB because no deterministic UCR/BG
    // strategy is in place. Matches tint_for_ink's vector treatment.
    let needs_4ch = matches!(resolved_space, ResolvedSpace::Cmyk | ResolvedSpace::IccCmyk);
    let needs_separation = matches!(resolved_space, ResolvedSpace::Separation(_));
    let needs_devicen = matches!(resolved_space, ResolvedSpace::DeviceN(_));
    if !(needs_4ch || needs_separation || needs_devicen) {
        log::debug!(
            "Skipping image XObject '{name}' on separation plates: \
             source colour space has no subtractive-ink intent"
        );
        return Ok(());
    }

    let pdf_image =
        match extract_image_from_xobject(Some(ctx.doc), xobject, obj_ref, Some(color_spaces)) {
            Ok(img) => img,
            Err(e) => {
                log::warn!("Skipping image XObject '{name}': {e}");
                return Ok(());
            }
        };
    let w = pdf_image.width() as usize;
    let h = pdf_image.height() as usize;
    let pixel_count = w * h;
    if pixel_count == 0 {
        return Ok(());
    }

    // §8.9.5: BitsPerComponent ∈ {1, 2, 4, 8, 16}. Channel extraction below
    // assumes 8 bits per sample (one byte per channel per pixel) and would
    // mis-read packed sub-byte or 16-bit streams. Until the routing path
    // supports full BPC expansion, skip with a log entry — matching the
    // JPX carve-out.
    let bpc = pdf_image.bits_per_component();
    if bpc != 8 {
        log::warn!(
            "Skipping image XObject '{name}' on separation plates: \
             BitsPerComponent={bpc} not supported (only 8-bpc channel \
             routing is implemented; 1/2/4/16-bpc expansion pending)"
        );
        return Ok(());
    }

    let extractor_cs = pdf_image.color_space();

    // Extract interleaved raw samples in the source colour space. The
    // extractor exposes RGB after Indexed expansion; for separation
    // routing we only consume CMYK / Separation / DeviceN paths (the
    // shapes above), so anything else falls through to skip.
    let (samples, stride) = match (resolved_space.clone(), extractor_cs, pdf_image.data()) {
        // Raw CMYK pixel buffer (Flate / CCITT / etc. on a DeviceCMYK image).
        (
            ResolvedSpace::Cmyk | ResolvedSpace::IccCmyk,
            PdfCs::DeviceCMYK | PdfCs::ICCBased(4),
            ImageData::Raw {
                pixels,
                format: PixelFormat::CMYK,
            },
        ) => (pixels.clone(), 4usize),
        // JPEG-encoded DeviceCMYK image — decode to raw CMYK preserving APP14 inversion.
        (
            ResolvedSpace::Cmyk | ResolvedSpace::IccCmyk,
            PdfCs::DeviceCMYK | PdfCs::ICCBased(4),
            ImageData::Jpeg(bytes),
        ) => (
            crate::extractors::images::decode_cmyk_jpeg_to_raw_cmyk(bytes)?,
            4,
        ),
        // Separation: 1 channel.
        (ResolvedSpace::Separation(_), PdfCs::Separation, ImageData::Raw { pixels, .. }) => {
            (pixels.clone(), 1)
        }
        // DeviceN: N channels (extractor reports DeviceN with N components).
        (ResolvedSpace::DeviceN(ref names), PdfCs::DeviceN, ImageData::Raw { pixels, .. }) => {
            (pixels.clone(), names.len().max(1))
        }
        // Shape mismatch (e.g. extractor reports a different colour space than
        // the dict declared after our resolver ran). Drop silently — the
        // resolver result wins for routing semantics but we won't fabricate
        // channels we don't have.
        _ => {
            log::debug!(
                "Image XObject '{name}': shape mismatch between resolved colour space \
                 and extractor sample format; skipping"
            );
            return Ok(());
        }
    };
    let _ = color_state; // currently unused outside the image-mask path

    // §8.9.5.2: /Decode maps raw sample values into the colour space's range.
    // For per-plate routing the colour space is treated as identity, so the
    // only effect that matters is inversion (`/Decode [1 0]` on a Separation
    // image, etc.). Default identity is `[0 1]` per channel.
    let decode = read_decode_array(dict, stride);

    let gs = gs_stack.current();
    let transform = combine_transforms(base_transform, &gs.ctm);

    for (i, &ink) in target_inks.iter().enumerate() {
        let Some(channel_idx) = image_channel_for_ink(&resolved_space, ink) else {
            continue;
        };
        if channel_idx >= stride {
            continue;
        }
        let mut plane = extract_image_channel(&samples, pixel_count, stride, channel_idx);
        if let Some(decode_pairs) = decode.as_ref() {
            if let Some(&(dmin, dmax)) = decode_pairs.get(channel_idx) {
                apply_decode_to_plane(&mut plane, dmin, dmax);
            }
        }
        blit_image_plane_to_plate(&mut pixmaps[i], &plane, w as u32, h as u32, transform, clip);
    }
    Ok(())
}

/// Expand a 1-bpc packed bitmap into one byte per pixel (0 or 255).
///
/// Per §8.9.5.1 each row is packed MSB-first into `ceil(width / 8)` bytes;
/// trailing bits in the final byte of each row are padding. Used to
/// normalise `/ImageMask true` stencils before they're blitted onto plates.
pub(super) fn expand_1bpc_to_8bpc(packed: &[u8], width: u32, height: u32) -> Vec<u8> {
    let row_bytes = width.div_ceil(8) as usize;
    let w = width as usize;
    let h = height as usize;
    let mut out = Vec::with_capacity(w * h);
    for row in 0..h {
        let row_start = row * row_bytes;
        for col in 0..w {
            let byte_idx = row_start + col / 8;
            let bit_idx = 7 - (col % 8);
            let bit = packed
                .get(byte_idx)
                .map(|b| (*b >> bit_idx) & 1)
                .unwrap_or(0);
            out.push(if bit == 1 { 255 } else { 0 });
        }
    }
    out
}

/// Read the image's `/Decode` array as per-channel `(dmin, dmax)` pairs.
/// Returns `None` if the entry is absent or malformed; callers fall back
/// to the identity mapping (no remap).
pub(super) fn read_decode_array(
    dict: &HashMap<String, Object>,
    num_components: usize,
) -> Option<Vec<(f32, f32)>> {
    let decode = dict.get("Decode")?;
    let arr = decode.as_array()?;
    if arr.len() < num_components * 2 {
        return None;
    }
    let to_f32 = |o: &Object| -> Option<f32> {
        match o {
            Object::Real(r) => Some(*r as f32),
            Object::Integer(i) => Some(*i as f32),
            _ => None,
        }
    };
    let mut out = Vec::with_capacity(num_components);
    for i in 0..num_components {
        let dmin = to_f32(&arr[i * 2])?;
        let dmax = to_f32(&arr[i * 2 + 1])?;
        out.push((dmin, dmax));
    }
    Some(out)
}

/// Apply a single channel's `/Decode` pair to an extracted 8-bpc plane.
///
/// For the identity mapping (`dmin = 0`, `dmax = 1`) this is a no-op. For
/// `[1 0]` this inverts the plane (raw 0 → 1.0 → 255, raw 255 → 0.0 → 0).
pub(super) fn apply_decode_to_plane(plane: &mut [u8], dmin: f32, dmax: f32) {
    if dmin == 0.0 && dmax == 1.0 {
        return;
    }
    for byte in plane.iter_mut() {
        let raw = *byte as f32 / 255.0;
        let decoded = (dmin + raw * (dmax - dmin)).clamp(0.0, 1.0);
        *byte = (decoded * 255.0).round() as u8;
    }
}

/// Paint an image mask (`/ImageMask true`) into the separation plates.
///
/// §8.9.6.2: the image samples are a 1-bpc stencil. The colour comes from
/// the current non-stroking graphics state, exactly as a vector fill would.
/// Each plate's tint comes from `tint_for_ink` for the current fill colour;
/// the stencil's alpha multiplies the paint into the destination.
#[allow(clippy::too_many_arguments)]
pub(super) fn paint_image_mask_to_plates(
    pixmaps: &mut [Pixmap],
    name: &str,
    xobject: &Object,
    obj_ref: Option<crate::object::ObjectRef>,
    base_transform: Transform,
    gs_stack: &GraphicsStateStack,
    color_state: Option<&SeparationColorState>,
    color_spaces: &HashMap<String, Object>,
    resources: &Object,
    ctx: &SeparationContext<'_>,
    clip: Option<&Mask>,
    target_inks: &[&str],
) -> Result<()> {
    // §8.9.6.2 ImageMask: the dict has no /ColorSpace, so the standard
    // image-extraction path rejects it. Read width/height/bpc directly from
    // the dict and decode the stream ourselves.
    let dict = match xobject {
        Object::Stream { dict, .. } => dict,
        _ => return Ok(()),
    };
    let w = dict.get("Width").and_then(|o| o.as_integer()).unwrap_or(0) as usize;
    let h = dict.get("Height").and_then(|o| o.as_integer()).unwrap_or(0) as usize;
    let pixel_count = w * h;
    if pixel_count == 0 {
        return Ok(());
    }
    let bpc = dict
        .get("BitsPerComponent")
        .and_then(|o| o.as_integer())
        .unwrap_or(1) as u8;
    if bpc != 1 {
        log::warn!(
            "Skipping image mask '{name}': BitsPerComponent={bpc} out of spec \
             (§8.9.6.2 mandates 1-bpc)"
        );
        return Ok(());
    }

    let packed = if let Some(r) = obj_ref {
        ctx.doc.decode_stream_with_encryption(xobject, r)?
    } else {
        xobject.decode_stream_data()?
    };
    let mut stencil = expand_1bpc_to_8bpc(&packed, w as u32, h as u32);
    if stencil.len() < pixel_count {
        return Ok(());
    }

    // §8.9.6.2: decoded sample value 0 marks the pixel with the current
    // colour; value 1 leaves it transparent. /Decode defaults to [0 1] —
    // applied here so /Decode [1 0] correctly inverts the stencil — then we
    // map decoded → alpha as `255 - decoded` so the existing SourceOver
    // composite paints where the spec says to paint.
    if let Some(decode_pairs) = read_decode_array(dict, 1) {
        if let Some(&(dmin, dmax)) = decode_pairs.first() {
            apply_decode_to_plane(&mut stencil, dmin, dmax);
        }
    }
    for byte in stencil.iter_mut() {
        *byte = 255 - *byte;
    }

    let gs = gs_stack.current();
    let transform = combine_transforms(base_transform, &gs.ctm);
    let empty = SeparationColorState::new();
    let cs = color_state.unwrap_or(&empty);

    for (i, &ink) in target_inks.iter().enumerate() {
        let PaintAction::Paint(tint) = tint_for_ink(
            true,
            gs,
            color_spaces,
            resources,
            ctx.doc,
            ink,
            &cs.fill_components,
            &cs.stroke_components,
        ) else {
            continue;
        };
        let gray = (tint.clamp(0.0, 1.0) * 255.0).round() as u8;

        // Build an RGBA buffer where R=G=B=gray and A=stencil_byte. SourceOver
        // composites this against the destination so opaque-stencil pixels
        // replace the plate value with `gray`; transparent-stencil pixels
        // leave the plate untouched.
        let mut rgba = Vec::with_capacity(pixel_count * 4);
        for &alpha in &stencil[..pixel_count] {
            rgba.extend_from_slice(&[gray, gray, gray, alpha]);
        }
        let Some(size) = tiny_skia::IntSize::from_wh(w as u32, h as u32) else {
            continue;
        };
        let Some(src) = Pixmap::from_vec(rgba, size) else {
            continue;
        };
        let image_transform = transform
            .pre_translate(0.0, 1.0)
            .pre_scale(1.0 / w as f32, -1.0 / h as f32);
        let mut paint = tiny_skia::PixmapPaint::default();
        paint.blend_mode = tiny_skia::BlendMode::SourceOver;
        paint.quality = tiny_skia::FilterQuality::Bilinear;
        pixmaps[i].draw_pixmap(0, 0, src.as_ref(), &paint, image_transform, clip);
    }
    Ok(())
}
