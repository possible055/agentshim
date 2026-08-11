use super::*;

impl PageRenderer {
    /// Render an image XObject.
    pub(super) fn render_image(
        &mut self,
        pixmap: &mut Pixmap,
        xobject: &Object,
        obj_ref: Option<ObjectRef>,
        transform: Transform,
        doc: &PdfDocument,
        clip_mask: Option<&tiny_skia::Mask>,
        smask_obj: Option<Object>,
        mask_obj: Option<Object>,
        gs: &GraphicsState,
    ) -> Result<()> {
        use crate::extractors::images::extract_image_from_xobject;

        // Use robust image extractor to handle various formats and color spaces
        let color_space_map = self.color_spaces.clone();
        let pdf_image =
            extract_image_from_xobject(Some(doc), xobject, obj_ref, Some(&color_space_map))?;
        let dynamic_image = pdf_image.to_dynamic_image()?;
        let mut rgba_image = dynamic_image.to_rgba8();

        // Handle /Mask (stencil mask image) — PDF spec section 8.9.6.2
        // The mask is a separate image whose samples define opacity (1=opaque, 0=transparent)
        if let Some(mask_ref) = mask_obj {
            if let Some(ref_obj) = mask_ref.as_reference() {
                if let Ok(mask_stream) = doc.load_object(ref_obj) {
                    // Try to decode the mask as an image
                    match extract_image_from_xobject(
                        Some(doc),
                        &mask_stream,
                        Some(ref_obj),
                        Some(&color_space_map),
                    ) {
                        Ok(mask_image) => {
                            if let Ok(mask_dyn) = mask_image.to_dynamic_image() {
                                let mask_gray = mask_dyn.to_luma8();
                                let mw = mask_gray.width();
                                let mh = mask_gray.height();
                                let iw = rgba_image.width();
                                let ih = rgba_image.height();
                                for y in 0..ih {
                                    for x in 0..iw {
                                        let mx = (x * mw / iw).min(mw - 1);
                                        let my = (y * mh / ih).min(mh - 1);
                                        let mask_val = mask_gray.get_pixel(mx, my)[0];
                                        let pixel = rgba_image.get_pixel_mut(x, y);
                                        pixel[3] =
                                            ((pixel[3] as u32 * mask_val as u32) / 255) as u8;
                                    }
                                }
                                log::debug!(
                                    "Applied image Mask ({}x{}) to image ({}x{})",
                                    mw,
                                    mh,
                                    iw,
                                    ih
                                );
                            }
                        }
                        Err(_) => {
                            // Fallback: decode stencil mask (ImageMask=true) directly from stream
                            if let Object::Stream { ref dict, .. } = mask_stream {
                                let mask_dict = dict;
                                let is_image_mask = mask_dict
                                    .get("ImageMask")
                                    .map(|o| matches!(o, Object::Boolean(true)))
                                    .unwrap_or(false);
                                if is_image_mask {
                                    let mw = mask_dict
                                        .get("Width")
                                        .and_then(|o| o.as_integer())
                                        .unwrap_or(0)
                                        as u32;
                                    let mh = mask_dict
                                        .get("Height")
                                        .and_then(|o| o.as_integer())
                                        .unwrap_or(0)
                                        as u32;
                                    if mw > 0 && mh > 0 {
                                        if let Ok(raw_mask_data) =
                                            doc.decode_stream_with_encryption(&mask_stream, ref_obj)
                                        {
                                            // CCITT data may be pass-through (not decompressed).
                                            // Check if we need to decompress Group 4 CCITT.
                                            let expected_bytes =
                                                ((mw as usize + 7) / 8) * mh as usize;
                                            let mask_data = if raw_mask_data.len()
                                                < expected_bytes / 2
                                            {
                                                // Data is still compressed — try Group 4 CCITT decompression
                                                let k = mask_dict
                                                    .get("DecodeParms")
                                                    .and_then(|o| o.as_dict())
                                                    .and_then(|d| d.get("K"))
                                                    .and_then(|o| o.as_integer())
                                                    .unwrap_or(0);
                                                if k == -1 {
                                                    #[allow(deprecated)]
                                                    let ccitt_result = crate::extractors::ccitt_bilevel::decompress_ccitt_group4(&raw_mask_data, mw, mh);
                                                    match ccitt_result {
                                                        Ok(decompressed) => {
                                                            log::debug!("CCITT Group4 decompressed mask: {} → {} bytes", raw_mask_data.len(), decompressed.len());
                                                            decompressed
                                                        }
                                                        Err(e) => {
                                                            log::debug!("CCITT decompression failed: {}, using raw data", e);
                                                            raw_mask_data
                                                        }
                                                    }
                                                } else {
                                                    raw_mask_data
                                                }
                                            } else {
                                                raw_mask_data
                                            };
                                            // 1-bit mask: each byte has 8 pixels, MSB first
                                            let iw = rgba_image.width();
                                            let ih = rgba_image.height();
                                            let row_bytes = (mw as usize + 7) / 8;
                                            for y in 0..ih {
                                                for x in 0..iw {
                                                    let mx = (x * mw / iw).min(mw - 1) as usize;
                                                    let my = (y * mh / ih).min(mh - 1) as usize;
                                                    let byte_idx = my * row_bytes + mx / 8;
                                                    let bit_idx = 7 - (mx % 8);
                                                    // PDF spec 8.9.6.2: mask bit 1 = paint (opaque), 0 = don't paint (transparent)
                                                    let mask_val = if byte_idx < mask_data.len() {
                                                        if (mask_data[byte_idx] >> bit_idx) & 1 == 1
                                                        {
                                                            255u8
                                                        } else {
                                                            0u8
                                                        }
                                                    } else {
                                                        255u8
                                                    };
                                                    let pixel = rgba_image.get_pixel_mut(x, y);
                                                    pixel[3] = ((pixel[3] as u32 * mask_val as u32)
                                                        / 255)
                                                        as u8;
                                                }
                                            }
                                            log::debug!("Applied stencil ImageMask ({}x{}) to image ({}x{})", mw, mh, iw, ih);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            } else if let Object::Array(mask_array) = &mask_ref {
                // Colour-key masking (ISO 32000-1 §8.9.6.4): the /Mask is an
                // array of 2 × ncomp integers [min1 max1 min2 max2 ...] in the
                // image's pre-Decode colour-component space. A source pixel whose
                // raw component samples all fall within their [min,max] range is
                // made fully transparent.
                let ncomp = pdf_image.color_space().components();
                match parse_color_key_mask(mask_array, ncomp) {
                    Some(ranges) => {
                        apply_color_key_mask(&pdf_image, &ranges, &mut rgba_image);
                    }
                    None => {
                        log::debug!("Ignoring malformed color-key /Mask array (ncomp={})", ncomp);
                    }
                }
            }
        }

        // Handle SMask if present
        if let Some(smask_ref) = smask_obj {
            if let Ok(resolved_smask) = doc.resolve_object(&smask_ref) {
                let smask_obj_ref = smask_ref.as_reference();
                if let Ok(smask_image) = extract_image_from_xobject(
                    Some(doc),
                    &resolved_smask,
                    smask_obj_ref,
                    Some(&color_space_map),
                ) {
                    if let Ok(smask_dyn) = smask_image.to_dynamic_image() {
                        let smask_gray = smask_dyn.to_luma8();

                        // Apply SMask to alpha channel
                        // Rescale smask if dimensions don't match (simplification)
                        let sw = smask_gray.width();
                        let sh = smask_gray.height();
                        let iw = rgba_image.width();
                        let ih = rgba_image.height();

                        for y in 0..ih {
                            for x in 0..iw {
                                // Map image coordinate to smask coordinate
                                let sx = (x * sw / iw).min(sw - 1);
                                let sy = (y * sh / ih).min(sh - 1);
                                let alpha = smask_gray.get_pixel(sx, sy)[0];

                                let pixel = rgba_image.get_pixel_mut(x, y);
                                // Combine with existing alpha
                                pixel[3] = ((pixel[3] as u32 * alpha as u32) / 255) as u8;
                            }
                        }
                    }
                }
            }
        }

        let src_w = rgba_image.width();
        let src_h = rgba_image.height();

        let image_transform = image_unit_square_transform(transform, src_w, src_h);
        let mut paint = pixmap_paint_for_image_blit(image_transform, gs.fill_alpha, &gs.blend_mode);

        // Fast path: SIMD pre-resize when the transform is a pure scale+translate and
        // the image is being downscaled.  fast_image_resize (AVX2/SSE4.1/NEON) resizes
        // to exact output dimensions; we then blit the already-correct pixels at the
        // right position with a translate-only transform and Nearest quality (no second
        // resampling pass).  For rotated/sheared transforms or upscaling, fall through
        // to the tiny-skia bilinear/bicubic path (already selected by the helper above).
        let use_fast = image_transform.kx.abs() <= 1e-4
            && image_transform.ky.abs() <= 1e-4
            && image_transform.sx > 0.0
            && image_transform.sy > 0.0
            && (image_transform.sx < 0.9 || image_transform.sy < 0.9);

        let (blit_w, blit_h, blit_data, blit_transform) = if use_fast {
            let dst_w = ((image_transform.sx * src_w as f32).round() as u32).max(1);
            let dst_h = ((image_transform.sy * src_h as f32).round() as u32).max(1);
            let resized = resize_rgba(rgba_image.as_raw(), src_w, src_h, dst_w, dst_h);
            if let Some(pixels) = resized {
                // SIMD pre-resize produced the exact output dimensions —
                // the subsequent blit is 1:1, so override to Nearest to
                // skip a second resampling pass.
                paint.quality = tiny_skia::FilterQuality::Nearest;
                let t = Transform::from_translate(image_transform.tx, image_transform.ty);
                (dst_w, dst_h, pixels, t)
            } else {
                // fast_image_resize failed; fall back to tiny_skia
                // resampling with the helper's chosen quality.
                (src_w, src_h, rgba_image.into_raw(), image_transform)
            }
        } else {
            // Rotated / sheared / upscaling path: let tiny_skia resample
            // with the helper's chosen quality.
            (src_w, src_h, rgba_image.into_raw(), image_transform)
        };

        if let Some(img_pixmap) = Pixmap::from_vec(
            blit_data,
            tiny_skia::IntSize::from_wh(blit_w, blit_h).unwrap(),
        ) {
            pixmap.draw_pixmap(0, 0, img_pixmap.as_ref(), &paint, blit_transform, clip_mask);
        }

        Ok(())
    }

    /// Return CCITT parameters for an ImageMask stream, if present.
    ///
    /// `/DecodeParms` arrays are positionally aligned with `/Filter`
    /// arrays. CCITT must be the final filter: earlier ASCII/Flate wrappers
    /// have already been removed by `decode_stream_with_encryption`, while a
    /// later filter would incorrectly receive still-compressed fax data from
    /// the stream decoder's intentional CCITT pass-through.
    pub(super) fn image_mask_ccitt_params(
        dict: &HashMap<String, Object>,
        width: u32,
        height: u32,
        doc: &PdfDocument,
    ) -> Result<Option<crate::decoders::CcittParams>> {
        let (ccitt_index, filter_count) = match dict.get("Filter") {
            Some(Object::Name(name)) if Self::is_ccitt_filter(name) => (0, 1),
            Some(Object::Array(filters)) => {
                let Some(index) = filters
                    .iter()
                    .position(|filter| filter.as_name().is_some_and(Self::is_ccitt_filter))
                else {
                    return Ok(None);
                };
                (index, filters.len())
            }
            _ => return Ok(None),
        };

        if ccitt_index + 1 != filter_count {
            return Err(Error::Image(
                "CCITTFaxDecode must be the final ImageMask filter".to_string(),
            ));
        }
        if width > u16::MAX as u32 {
            return Err(Error::Image(format!(
                "CCITT ImageMask width {width} exceeds decoder limit {}",
                u16::MAX
            )));
        }

        let resolved_params = dict
            .get("DecodeParms")
            .map(|params| {
                let resolved = doc.resolve_object(params).map_err(|error| {
                    Error::Image(format!(
                        "Unable to resolve CCITT ImageMask /DecodeParms: {error}"
                    ))
                })?;
                if params.as_reference().is_some() && matches!(&resolved, Object::Null) {
                    return Err(Error::Image(
                        "Unable to resolve CCITT ImageMask /DecodeParms: reference resolved to null"
                            .to_string(),
                    ));
                }
                Ok(resolved)
            })
            .transpose()?;
        let selected_params = match resolved_params {
            Some(Object::Array(params)) => params
                .get(ccitt_index)
                .map(|params| {
                    let resolved = doc.resolve_object(params).map_err(|error| {
                        Error::Image(format!(
                            "Unable to resolve CCITT ImageMask /DecodeParms array entry: {error}"
                        ))
                    })?;
                    if params.as_reference().is_some() && matches!(&resolved, Object::Null) {
                        return Err(Error::Image(
                            "Unable to resolve CCITT ImageMask /DecodeParms array entry: reference resolved to null"
                                .to_string(),
                        ));
                    }
                    Ok(resolved)
                })
                .transpose()?,
            other => other,
        };
        let params_obj = selected_params.as_ref();
        let params_dict = match params_obj {
            None | Some(Object::Null) => None,
            Some(params) => Some(params.as_dict().ok_or_else(|| {
                Error::Image("CCITT ImageMask /DecodeParms must be a dictionary".to_string())
            })?),
        };
        if params_dict
            .and_then(|decode_params| decode_params.get("K"))
            .is_some_and(|value| value.as_integer().is_none())
        {
            return Err(Error::Image(
                "CCITT ImageMask /DecodeParms /K must be an integer".to_string(),
            ));
        }
        Self::validate_ccitt_mask_dimension(params_dict, "Columns", width)?;
        Self::validate_ccitt_mask_dimension(params_dict, "Rows", height)?;

        let mut params = crate::object::extract_ccitt_params_with_width(params_obj, Some(width))
            .unwrap_or_else(|| crate::decoders::CcittParams {
                // ISO 32000-1 Table 11: absent /K defaults to Group 3 1-D.
                k: 0,
                columns: width,
                rows: Some(height),
                ..Default::default()
            });
        let has_explicit_k = params_obj
            .and_then(Object::as_dict)
            .is_some_and(|decode_params| decode_params.contains_key("K"));
        if !has_explicit_k {
            // The shared parser currently carries a historical Group 4
            // default; enforce the PDF filter default at this renderer
            // boundary without changing other image-extraction behaviour.
            params.k = 0;
        }
        // Width and Height are authoritative for an Image XObject. Explicit
        // mismatches were rejected above; normalise omitted values here.
        params.columns = width;
        params.rows = Some(height);
        Ok(Some(params))
    }

    pub(super) fn validate_ccitt_mask_dimension(
        params: Option<&HashMap<String, Object>>,
        key: &str,
        expected: u32,
    ) -> Result<()> {
        let Some(value) = params.and_then(|decode_params| decode_params.get(key)) else {
            return Ok(());
        };
        let integer = value.as_integer().ok_or_else(|| {
            Error::Image(format!(
                "CCITT ImageMask /DecodeParms /{key} must be an integer"
            ))
        })?;
        let actual = u32::try_from(integer).map_err(|_| {
            Error::Image(format!(
                "CCITT ImageMask /DecodeParms /{key} must be positive"
            ))
        })?;
        if actual == 0 {
            return Err(Error::Image(format!(
                "CCITT ImageMask /DecodeParms /{key} must be positive"
            )));
        }
        if actual != expected {
            return Err(Error::Image(format!(
                "CCITT ImageMask /DecodeParms /{key} {actual} does not match image dimension {expected}"
            )));
        }
        Ok(())
    }

    pub(super) fn is_ccitt_filter(name: &str) -> bool {
        name.eq_ignore_ascii_case("CCITTFaxDecode") || name.eq_ignore_ascii_case("CCF")
    }

    pub(super) fn image_mask_layout(
        dict: &HashMap<String, Object>,
    ) -> Result<(u32, u32, usize, usize, usize)> {
        let dimension = |key: &str| -> Result<u32> {
            let value = dict
                .get(key)
                .and_then(Object::as_integer)
                .ok_or_else(|| Error::Image(format!("ImageMask missing /{key}")))?;
            let dimension = u32::try_from(value)
                .map_err(|_| Error::Image(format!("ImageMask /{key} must be positive")))?;
            if dimension == 0 {
                return Err(Error::Image(format!("ImageMask /{key} must be positive")));
            }
            Ok(dimension)
        };

        let width = dimension("Width")?;
        let height = dimension("Height")?;
        let width_usize = usize::try_from(width)
            .map_err(|_| Error::Image("ImageMask /Width exceeds platform limits".to_string()))?;
        let height_usize = usize::try_from(height)
            .map_err(|_| Error::Image("ImageMask /Height exceeds platform limits".to_string()))?;
        let pixel_count = width_usize
            .checked_mul(height_usize)
            .ok_or_else(|| Error::Image("ImageMask pixel count overflow".to_string()))?;
        let rgba_len = pixel_count
            .checked_mul(4)
            .ok_or_else(|| Error::Image("ImageMask RGBA size overflow".to_string()))?;
        let row_bytes = width_usize
            .checked_add(7)
            .ok_or_else(|| Error::Image("ImageMask row size overflow".to_string()))?
            / 8;
        let expected = row_bytes
            .checked_mul(height_usize)
            .ok_or_else(|| Error::Image("ImageMask packed size overflow".to_string()))?;
        Ok((width, height, row_bytes, expected, rgba_len))
    }

    /// Render an Image XObject with `/ImageMask true` — a 1-bit stencil
    /// painted with the current fill colour.
    ///
    /// Per ISO 32000-1 §8.9.6.4, under the default `/Decode [0 1]` a
    /// sample value of `0` paints the destination with the current
    /// nonstroking colour and `1` leaves it unaffected; `/Decode [1 0]`
    /// reverses the polarity. There is no `/ColorSpace`; the colour
    /// comes from `gs.fill_color_rgb` / `gs.fill_alpha`. The caller (the
    /// `Do` arm in `render_page_with_options`) is responsible for
    /// routing that fill through the resolution pipeline, so this
    /// helper consumes whatever `gs` it is handed without re-resolving.
    ///
    /// Supports raw 1-bit samples and CCITT Group 3/4 streams, default
    /// and inverted `/Decode` polarities, and bilinear/bicubic resampling
    /// chosen by the image-space-to-user-space scale (matches
    /// `render_image`).
    pub(super) fn render_image_mask(
        &mut self,
        pixmap: &mut Pixmap,
        xobject: &Object,
        obj_ref: Option<ObjectRef>,
        transform: Transform,
        doc: &PdfDocument,
        clip_mask: Option<&tiny_skia::Mask>,
        gs: &GraphicsState,
    ) -> Result<()> {
        let dict = xobject
            .as_dict()
            .ok_or_else(|| Error::Image("ImageMask XObject is not a stream".to_string()))?;

        let (width, height, row_bytes, expected, rgba_len) = Self::image_mask_layout(dict)?;

        // PDF §8.9.6.4: ImageMask BitsPerComponent must be 1 when present.
        // Some producers omit it; default to 1.
        let bpc = dict
            .get("BitsPerComponent")
            .and_then(|o| o.as_integer())
            .unwrap_or(1);
        if bpc != 1 {
            return Err(Error::Image(format!(
                "ImageMask requires BitsPerComponent 1, got {bpc}"
            )));
        }

        // /Decode array: [0 1] means sample 0 paints (default); [1 0]
        // means sample 1 paints. Other forms are spec-illegal for ImageMask.
        let invert = match dict.get("Decode") {
            Some(Object::Array(arr)) if arr.len() >= 2 => {
                let first = match &arr[0] {
                    Object::Real(v) => *v as f32,
                    Object::Integer(v) => *v as f32,
                    _ => 0.0,
                };
                first > 0.5
            }
            _ => false,
        };

        let mut raw = if let Some(r) = obj_ref {
            doc.decode_stream_with_encryption(xobject, r)?
        } else {
            xobject.decode_stream_data()?
        };
        if let Some(params) = Self::image_mask_ccitt_params(dict, width, height, doc)? {
            raw = crate::extractors::ccitt_bilevel::decompress_ccitt(&raw, &params)?;

            // The shared CCITT image decoder normalises packed rows to
            // 0=white, 1=black. An ImageMask needs the actual PDF sample
            // values instead: with the CCITT default `/BlackIs1 false`,
            // 0 means black and 1 means white; `/BlackIs1 true` reverses
            // that mapping. `decompress_ccitt` already applies BlackIs1
            // while normalising, so one final inversion recovers the sample
            // values consumed by the independent `/Decode` mapping below.
            for byte in &mut raw {
                *byte = !*byte;
            }
        }

        // Stencil pixels → premultiplied RGBA, applying the fill colour
        // to each opaque sample. Rows are packed MSB-first; each row is
        // padded to the next byte boundary.
        let (fr, fg, fb) = gs.fill_color_rgb;
        let fa = gs.fill_alpha.clamp(0.0, 1.0);
        let pa = (fa * 255.0).round().clamp(0.0, 255.0) as u8;
        // Premultiplied opaque sample: tiny-skia's Pixmap is
        // premultiplied; build the channels accordingly so blends and
        // SMask composition stay correct.
        let pr = ((fr.clamp(0.0, 1.0) * fa) * 255.0)
            .round()
            .clamp(0.0, 255.0) as u8;
        let pg = ((fg.clamp(0.0, 1.0) * fa) * 255.0)
            .round()
            .clamp(0.0, 255.0) as u8;
        let pb = ((fb.clamp(0.0, 1.0) * fa) * 255.0)
            .round()
            .clamp(0.0, 255.0) as u8;

        if raw.len() < expected {
            return Err(Error::Image(format!(
                "ImageMask stream too short: {} bytes for {}x{} (expected {})",
                raw.len(),
                width,
                height,
                expected
            )));
        }

        let mut rgba = Vec::new();
        rgba.try_reserve_exact(rgba_len)
            .map_err(|_| Error::Image(format!("Unable to allocate {rgba_len} ImageMask bytes")))?;
        rgba.resize(rgba_len, 0);
        for y in 0..height {
            let row_off = (y as usize) * row_bytes;
            for x in 0..width {
                let byte_idx = row_off + (x / 8) as usize;
                let bit_idx = 7 - (x % 8);
                let bit = (raw[byte_idx] >> bit_idx) & 1 == 1;
                let opaque = if invert { bit } else { !bit };
                if opaque {
                    let off = ((y as usize * width as usize) + x as usize) * 4;
                    rgba[off] = pr;
                    rgba[off + 1] = pg;
                    rgba[off + 2] = pb;
                    rgba[off + 3] = pa;
                }
            }
        }

        let image_transform = image_unit_square_transform(transform, width, height);
        // Opacity is 1.0 because fill_alpha is already baked into the
        // stencil pixels by the loop above; blend mode + scale-driven
        // quality come from the shared helper.
        let paint = pixmap_paint_for_image_blit(image_transform, 1.0, &gs.blend_mode);

        if let Some(stencil_pixmap) = Pixmap::from_vec(
            rgba,
            tiny_skia::IntSize::from_wh(width, height)
                .ok_or_else(|| Error::Image("ImageMask invalid dimensions".to_string()))?,
        ) {
            pixmap.draw_pixmap(
                0,
                0,
                stencil_pixmap.as_ref(),
                &paint,
                image_transform,
                clip_mask,
            );
        }

        Ok(())
    }
}
