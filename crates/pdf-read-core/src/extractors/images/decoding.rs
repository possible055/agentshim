use super::*;

/// Extract an image from an XObject stream.
pub fn extract_image_from_xobject(
    doc: Option<&crate::document::PdfDocument>,
    xobject: &crate::object::Object,
    obj_ref: Option<ObjectRef>,
    color_space_map: Option<&std::collections::HashMap<String, crate::object::Object>>,
) -> Result<PdfImage> {
    use crate::object::Object;

    let dict = xobject
        .as_dict()
        .ok_or_else(|| Error::Image("XObject is not a stream".to_string()))?;

    let subtype = dict
        .get("Subtype")
        .and_then(|obj| obj.as_name())
        .ok_or_else(|| Error::Image("XObject missing /Subtype".to_string()))?;

    if subtype != "Image" {
        return Err(Error::Image(format!(
            "XObject subtype is not Image: {}",
            subtype
        )));
    }

    let width = dict
        .get("Width")
        .and_then(|obj| obj.as_integer())
        .ok_or_else(|| Error::Image("Image missing /Width".to_string()))? as u32;

    let height = dict
        .get("Height")
        .and_then(|obj| obj.as_integer())
        .ok_or_else(|| Error::Image("Image missing /Height".to_string()))? as u32;

    let bits_per_component = dict
        .get("BitsPerComponent")
        .and_then(|obj| obj.as_integer())
        .unwrap_or(8) as u8;

    // Declared geometry is attacker-controlled and is used to size buffers further down,
    // so it is checked here, before anything is allocated from it. A dictionary claiming
    // 100000x100000 costs nothing to write and 30 GB to honour.
    crate::budget::check_image_dimensions(width, height, bits_per_component)?;

    let color_space_obj = dict
        .get("ColorSpace")
        .ok_or_else(|| Error::Image("Image missing /ColorSpace".to_string()))?;

    let resolved_color_space = if let Some(d) = doc {
        let res = if let Some(obj_ref) = color_space_obj.as_reference() {
            d.load_object(obj_ref)?
        } else {
            color_space_obj.clone()
        };
        if let Object::Name(ref name) = res {
            if let Some(map) = color_space_map {
                map.get(name).cloned().unwrap_or(res)
            } else {
                res
            }
        } else {
            res
        }
    } else {
        color_space_obj.clone()
    };

    // For array-form color spaces (e.g. [/ICCBased <ref>], [/Indexed <base> <hi> <palette_ref>])
    // the second element is commonly an indirect reference to the ICC profile
    // stream / palette. `parse_color_space` only inspects the immediate
    // `Object::Stream` dict, so an unresolved reference silently falls back to
    // `N = 3` and a CMYK (N = 4) image is labelled as RGB. Resolve the stream
    // reference here so the component count reflects the real profile.
    let resolved_color_space =
        if let (Some(doc_mut), Object::Array(arr)) = (doc, &resolved_color_space) {
            if arr.len() > 1 {
                if let Some(second_ref) = arr[1].as_reference() {
                    if let Ok(resolved_second) = doc_mut.load_object(second_ref) {
                        let mut new_arr = arr.clone();
                        new_arr[1] = resolved_second;
                        Object::Array(new_arr)
                    } else {
                        resolved_color_space
                    }
                } else {
                    resolved_color_space
                }
            } else {
                resolved_color_space
            }
        } else {
            resolved_color_space
        };

    let color_space = parse_color_space(&resolved_color_space)?;
    // For Indexed color spaces, resolve the base color space and palette now so we
    // can expand indices to RGB after decoding the stream. Without this, raw
    // Indexed pixel data (1 byte per pixel) is mislabelled as RGB (3 bytes per
    // pixel) and ImageBuffer::from_raw rejects the wrong length. Fail fast if
    // the palette cannot be resolved so the error points at the real root cause
    // instead of the downstream "Invalid RGB image dimensions" symptom.
    let indexed_resolution: Option<IndexedResolution> = if color_space == ColorSpace::Indexed {
        let resolved = resolve_indexed_palette(doc, &resolved_color_space)?;
        if resolved.is_none() {
            return Err(Error::Image(
                "Unable to resolve Indexed color space palette".to_string(),
            ));
        }
        resolved
    } else {
        None
    };

    // For a plain (non-Indexed) `[/ICCBased <stream>]` colour space,
    // capture the profile bytes so the CMM can convert through the
    // document's actual source characterisation instead of the
    // §10.3.5 additive-clamp fallback.
    //
    // When the image uses plain `/DeviceCMYK` with no ICC profile of
    // its own, fall back to the document's `/OutputIntents` CMYK
    // profile if one exists — the standard PDF/X assumption per
    // ISO 32000-1:2008 §14.11.5.
    let direct_icc_profile = if matches!(color_space, ColorSpace::ICCBased(_)) {
        resolve_icc_profile_from_obj(doc, &resolved_color_space)
    } else if color_space == ColorSpace::DeviceCMYK {
        doc.and_then(|d| d.output_intent_cmyk_profile())
    } else {
        None
    };

    // Per §8.6.5.8, an image dictionary may override the graphics-state
    // rendering intent via `/Intent`. Unrecognised names fall through
    // to `RelativeColorimetric`.
    let rendering_intent = dict
        .get("Intent")
        .and_then(|obj| obj.as_name())
        .map(crate::color::RenderingIntent::from_pdf_name)
        .unwrap_or_default();

    let filter_names = if let Some(filter_obj) = dict.get("Filter") {
        match filter_obj {
            Object::Name(name) => vec![name.clone()],
            Object::Array(filters) => filters
                .iter()
                .filter_map(|f| f.as_name().map(String::from))
                .collect(),
            _ => vec![],
        }
    } else {
        vec![]
    };

    let has_dct = filter_names.iter().any(|name| name == "DCTDecode");
    let is_jpeg_only = has_dct && filter_names.len() == 1;
    let is_jpeg_chain = has_dct && filter_names.len() > 1;

    let is_jbig2 = filter_names
        .iter()
        .any(|n| n.eq_ignore_ascii_case("JBIG2Decode"));

    let is_jpx = filter_names
        .iter()
        .any(|n| n.eq_ignore_ascii_case("JPXDecode"));

    let is_ccitt = filter_names
        .iter()
        .any(|n| n.eq_ignore_ascii_case("CCITTFaxDecode"));

    let data = if is_jbig2 {
        decode_jbig2_image(xobject, obj_ref, dict, doc, width, height)?
    } else if is_jpx {
        decode_jpx_image(xobject, obj_ref, doc, &color_space)?
    } else if is_jpeg_only || is_jpeg_chain {
        let decoded = if let (Some(d), Some(ref_id)) = (doc.as_ref(), obj_ref) {
            d.decode_stream_with_encryption(xobject, ref_id)?
        } else {
            xobject.decode_stream_data()?
        };
        ImageData::Jpeg(decoded)
    } else {
        let decoded_data = if let (Some(d), Some(ref_id)) = (doc.as_ref(), obj_ref) {
            d.decode_stream_with_encryption(xobject, ref_id)?
        } else {
            xobject.decode_stream_data()?
        };
        if let Some(ir) = indexed_resolution.as_ref() {
            // Build a Transform if the Indexed base has a profile so
            // palette entries render through the real CMM (when linked).
            let transform = ir
                .base_profile
                .clone()
                .map(|p| crate::color::Transform::new_srgb_target(p, rendering_intent));
            let expanded = expand_indexed_to_rgb_with_transform(
                &decoded_data,
                &ir.palette,
                ir.base_fmt,
                width,
                height,
                bits_per_component,
                transform.as_ref(),
            )?;
            ImageData::Raw {
                pixels: expanded,
                format: PixelFormat::RGB,
            }
        } else {
            let pixel_format = color_space_to_pixel_format(&color_space);
            // ISO 32000-1 §8.9.5.2: BitsPerComponent 16 stores each colour
            // sample as a big-endian 16-bit value. The rest of the image
            // pipeline (and the render target, an 8-bit tiny_skia pixmap)
            // assumes 8-bit samples, so reduce each sample to 8 bits. Without
            // this the raw buffer is twice the expected length; the lenient
            // `ImageBuffer::from_raw` then builds an oversized image whose
            // buffer the PNG encoder rejects with a panic (`assertion
            // left == right failed: Invalid buffer length`).
            //
            // The reduction rounds `v * 255 / 65535` to the nearest 8-bit
            // value rather than dropping the low byte (`v >> 8`, i.e. floor):
            // truncation biases every sample downward by up to ~1 LSB, most
            // visibly darkening near-white highlights. Full u16 precision
            // through extraction is deferred (v0.3.72) — no current consumer
            // benefits, as both the PNG path and the rasteriser are 8-bit.
            let pixels = if bits_per_component == 16 {
                decoded_data
                    .chunks_exact(2)
                    .map(|sample| reduce_16_to_8(sample[0], sample[1]))
                    .collect()
            } else if bits_per_component == 1
                && color_space == ColorSpace::DeviceGray
                && !is_ccitt
                && decode_array_inverts_1bpc(dict.get("Decode"))
            {
                // Fold a non-default /Decode [1 0] into the packed bits here,
                // the same way the CCITT branch below folds it into
                // `black_is_1`, so `to_dynamic_image` can unpack with fixed
                // (non-inverted) bit semantics regardless of /Decode. Flipping
                // every bit of a 1-bpp buffer is a plain byte-wise NOT; the
                // unused row-padding bits get flipped too but are never read.
                decoded_data.iter().map(|b| !b).collect()
            } else {
                decoded_data
            };
            ImageData::Raw {
                pixels,
                format: pixel_format,
            }
        }
    };

    // JBIG2 decode produces 8-bit-per-channel pixels regardless of the
    // XObject's BitsPerComponent (which is 1).  Override to 8 so that
    // to_dynamic_image() does not try to CCITT-decompress the output.
    // 16-bit samples were collapsed to their high byte above (and an Indexed
    // base always expands to 8-bit RGB), so the stored pixels are 8-bit per
    // component in those cases too.
    let effective_bpc = if is_jbig2 || bits_per_component == 16 {
        8
    } else {
        bits_per_component
    };
    let mut image = PdfImage::new(width, height, color_space, effective_bpc, data);

    // Attach the ICC profile if we found one — prefer the direct ICCBased
    // profile, then fall back to an Indexed base's profile so the CMM has
    // something to work with for palette-backed CMYK/Lab images too.
    if let Some(p) = direct_icc_profile {
        image.set_icc_profile(p);
    } else if let Some(ir) = indexed_resolution.as_ref() {
        if let Some(p) = ir.base_profile.clone() {
            image.set_icc_profile(p);
        }
    }
    image.set_rendering_intent(rendering_intent);

    if bits_per_component == 1 && image.color_space() == &ColorSpace::DeviceGray && is_ccitt {
        if let Some(mut ccitt_params) =
            crate::object::extract_ccitt_params_with_width(dict.get("DecodeParms"), Some(width))
        {
            if ccitt_params.rows.is_none() {
                ccitt_params.rows = Some(height);
            }
            // ISO 32000-1 7.4.6 /BlackIs1 and 8.9.5.2 Table 90 /Decode both
            // flip which sample bit means "black" for a 1-bit DeviceGray
            // image, and they are independent, composable mechanisms: a
            // producer that wants inverted polarity may set /BlackIs1 true
            // *or* write /Decode [1 0] on the image XObject instead (both
            // are seen in real-world scanned PDFs). Fold /Decode into the
            // same inversion flag decompress_ccitt already honors so either
            // one alone inverts and both together cancel out.
            if decode_array_inverts_1bpc(dict.get("Decode")) {
                ccitt_params.black_is_1 = !ccitt_params.black_is_1;
            }
            image.set_ccitt_params(ccitt_params);
        }
    }

    Ok(image)
}

/// Extract and parse an `ICCBased` colour-space's profile stream.
///
/// Accepts either a fully-resolved `[/ICCBased <Stream>]` array (the
/// stream is an `Object::Stream` directly), or a `[/ICCBased <Ref>]`
/// array where the second element is a live reference — in that case
/// `doc` must be supplied so we can dereference.
///
/// Returns `None` if:
///   - `cs_obj` isn't an ICCBased array,
///   - the profile stream can't be decoded,
///   - the profile bytes fail ICC header validation, or
///   - the declared `/N` disagrees with the profile header's
///     colourSpace signature (PDF §8.6.5.5 mandates they match).
///
/// No error is returned — callers treat "no profile" as "fall back to
/// device colour space" per §8.6.5.5's /Alternate clause.
pub(crate) fn resolve_icc_profile_from_obj(
    doc: Option<&crate::document::PdfDocument>,
    cs_obj: &crate::object::Object,
) -> Option<std::sync::Arc<crate::color::IccProfile>> {
    use crate::object::Object;

    let Object::Array(arr) = cs_obj else {
        return None;
    };
    if arr.len() < 2 || arr[0].as_name() != Some("ICCBased") {
        return None;
    }

    // Second element should be a stream (already resolved by the caller
    // in the common path) or a reference we still need to dereference.
    let profile_obj = match (&arr[1], doc) {
        (Object::Stream { .. }, _) => arr[1].clone(),
        (Object::Reference(r), Some(d)) => match d.load_object(*r) {
            Ok(obj) => obj,
            Err(_) => return None,
        },
        _ => return None,
    };

    let Object::Stream { dict, .. } = &profile_obj else {
        return None;
    };
    // `N` is mandatory per PDF 32000-1 §8.6.5.5 Table 66.
    let n = dict
        .get("N")
        .and_then(|obj| obj.as_integer())
        .filter(|n| matches!(*n, 1 | 3 | 4))? as u8;

    let bytes = profile_obj.decode_stream_data().ok()?;
    let profile = crate::color::IccProfile::parse(bytes, n)?;
    Some(std::sync::Arc::new(profile))
}
