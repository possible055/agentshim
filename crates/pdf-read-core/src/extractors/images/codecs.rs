use super::*;

/// Convert a raw CMYK byte stream (4 bytes per pixel) to straight RGB bytes
/// (3 bytes per pixel) using the naive per-pixel conversion.
///
/// This is a non-ICC conversion and does not handle Adobe-inverted JPEG CMYK;
/// for JPEG-encoded CMYK streams use `decode_adobe_cmyk_jpeg` instead.
pub fn cmyk_to_rgb(cmyk: &[u8]) -> Vec<u8> {
    cmyk_to_rgb_with_transform(cmyk, None)
}

/// Like [`cmyk_to_rgb`] but routes through an ICC transform when given,
/// and falls through to §10.3.5 otherwise. Used by save_raw_as_* when
/// the source image carries an ICC profile.
pub fn cmyk_to_rgb_with_transform(
    cmyk: &[u8],
    transform: Option<&crate::color::Transform>,
) -> Vec<u8> {
    if let Some(t) = transform {
        return t.convert_cmyk_buffer(cmyk);
    }
    let mut rgb = Vec::with_capacity((cmyk.len() / 4) * 3);
    for chunk in cmyk.chunks_exact(4) {
        let [r, g, b] = cmyk_pixel_to_rgb(chunk[0], chunk[1], chunk[2], chunk[3]);
        rgb.push(r);
        rgb.push(g);
        rgb.push(b);
    }
    rgb
}

/// Decode a CMYK-colourspace JPEG to straight RGB bytes, applying Adobe's
/// Thin wrapper that falls back to the intent-less, profile-less
/// variant — kept as the public, backwards-compatible entry point.
pub fn decode_cmyk_jpeg_to_rgb(jpeg_data: &[u8]) -> Result<Vec<u8>> {
    decode_cmyk_jpeg_to_rgb_with_profile(jpeg_data, None)
}

/// Decode a DeviceCMYK JPEG to raw 8-bpc CMYK samples (W*H*4 bytes,
/// channel order C, M, Y, K). Output is the raw DCT sample plane treated
/// as straight CMYK ink (0 = no ink, 255 = full coverage), matching how
/// poppler / Ghostscript render DCTDecode CMYK streams inside a PDF.
///
/// `jpeg-decoder` 0.3 applies Adobe's `255 - x` inversion to EVERY
/// 4-component JPEG that carries an Adobe APP14 marker
/// (`color_convert_line_cmyk` for `transform = 0`; `color_convert_line_ycck`
/// for `transform = 2`). PDF renderers do not do that inversion - they use
/// the raw DCT samples as the CMYK ink. So whenever an Adobe marker is
/// present, pdf_oxide must undo the decoder's inversion with a second
/// `255 - x` to recover the raw samples:
///
/// - `color_transform = 0` (plain CMYK, Photoshop / Distiller default) -
///   undo the decoder's `color_convert_line_cmyk` inversion.
/// - `color_transform = 2` (YCCK, Adobe Illustrator default) - the decoder
///   ran YCbCr->RGB plus `255 - K`; the same `255 - x` on all four channels
///   recovers the raw CMYK plane.
/// - no APP14 marker - the samples are used as-is (non-Adobe convention),
///   left unchanged to avoid disturbing non-Adobe JPEG handling.
///
/// The jpeg-decoder inversion contract is pinned by
/// `tests/test_jpeg_decoder_cmyk_contract.rs`; the Adobe decode path is
/// covered by `tests/test_cmyk_jpeg_adobe_inversion.rs`. If a future
/// jpeg-decoder release changes its inversion behaviour, those tests fire
/// before real fixtures regress.
///
/// Used by the separation pipeline to route CMYK image channels directly
/// to the matching ink plates without going through a colour-space
/// conversion to RGB and back. Only the rendering feature consumes this;
/// without it the function would be dead code under `-D warnings`.
#[cfg(feature = "rendering")]
pub(crate) fn decode_cmyk_jpeg_to_raw_cmyk(jpeg_data: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = jpeg_decoder::Decoder::new(std::io::Cursor::new(jpeg_data));
    let cmyk = decoder
        .decode()
        .map_err(|e| Error::Decode(format!("Failed to decode CMYK JPEG: {}", e)))?;
    let info = decoder
        .info()
        .ok_or_else(|| Error::Decode("JPEG info unavailable".to_string()))?;

    let pixel_count = (info.width as usize) * (info.height as usize);
    let expected = pixel_count * 4;
    if cmyk.len() < expected {
        return Err(Error::Decode(format!(
            "CMYK JPEG decoded {} bytes, expected {}",
            cmyk.len(),
            expected
        )));
    }

    let mut raw = cmyk;
    raw.truncate(expected);
    // An Adobe APP14 marker (transform 0 = CMYK, 2 = YCCK) means jpeg-decoder
    // has already applied a `255 - x` inversion; undo it to recover the raw
    // DCT samples poppler uses as straight CMYK ink.
    if matches!(scan_app14_color_transform(jpeg_data), Some(0) | Some(2)) {
        for b in raw.iter_mut() {
            *b = 255 - *b;
        }
    }
    Ok(raw)
}

/// Like [`decode_cmyk_jpeg_to_rgb`] but applies the given ICC transform
/// when provided, falling back to §10.3.5 otherwise. Used internally by
/// `PdfImage::save_as_*` when the source image carries an ICCBased
/// colour space (or when the document's `OutputIntents` supplied a
/// default CMYK profile).
///
/// APP14 handling matches `decode_cmyk_jpeg_to_raw_cmyk`: when an Adobe
/// marker is present (CMYK transform 0 or YCCK transform 2), jpeg-decoder's
/// `255 - x` inversion is undone to recover the raw DCT samples poppler
/// treats as straight CMYK; without a marker the samples pass through.
pub fn decode_cmyk_jpeg_to_rgb_with_profile(
    jpeg_data: &[u8],
    transform: Option<&crate::color::Transform>,
) -> Result<Vec<u8>> {
    let mut decoder = jpeg_decoder::Decoder::new(std::io::Cursor::new(jpeg_data));
    let cmyk = decoder
        .decode()
        .map_err(|e| Error::Decode(format!("Failed to decode CMYK JPEG: {}", e)))?;
    let info = decoder
        .info()
        .ok_or_else(|| Error::Decode("JPEG info unavailable".to_string()))?;

    let pixel_count = (info.width as usize) * (info.height as usize);
    let expected = pixel_count * 4;
    if cmyk.len() < expected {
        return Err(Error::Decode(format!(
            "CMYK JPEG decoded {} bytes, expected {}",
            cmyk.len(),
            expected
        )));
    }

    // Undo jpeg-decoder's Adobe `255 - x` inversion (applied for any APP14
    // CMYK transform 0 or YCCK transform 2) to recover the raw DCT samples,
    // which poppler / Ghostscript render as straight CMYK ink. No marker ->
    // pass through unchanged.
    let straight_cmyk: Vec<u8> =
        if matches!(scan_app14_color_transform(jpeg_data), Some(0) | Some(2)) {
            cmyk[..expected].iter().map(|b| 255 - *b).collect()
        } else {
            cmyk[..expected].to_vec()
        };

    if let Some(t) = transform {
        return Ok(t.convert_cmyk_buffer(&straight_cmyk));
    }

    // §10.3.5 additive-clamp fallback.
    let mut rgb = Vec::with_capacity(pixel_count * 3);
    for chunk in straight_cmyk.chunks_exact(4) {
        let [r, g, b] = cmyk_pixel_to_rgb(chunk[0], chunk[1], chunk[2], chunk[3]);
        rgb.push(r);
        rgb.push(g);
        rgb.push(b);
    }
    Ok(rgb)
}

/// Walk the JPEG marker stream for an Adobe APP14 ("Adobe") segment and
/// return its `color_transform` byte. The byte is 0 for plain CMYK
/// (Photoshop), 1 for YCbCr (3-channel), 2 for YCCK (Adobe Illustrator).
fn scan_app14_color_transform(jpeg_data: &[u8]) -> Option<u8> {
    let mut i = 0;
    while i + 1 < jpeg_data.len() {
        if jpeg_data[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = jpeg_data[i + 1];
        i += 2;
        if marker == 0x00 || marker == 0xFF {
            continue;
        }
        if matches!(marker, 0xD0..=0xD9) || marker == 0x01 {
            continue;
        }
        if i + 1 >= jpeg_data.len() {
            break;
        }
        let seg_len = u16::from_be_bytes([jpeg_data[i], jpeg_data[i + 1]]) as usize;
        if seg_len < 2 || i + seg_len > jpeg_data.len() {
            break;
        }
        if marker == 0xEE && seg_len >= 14 {
            let payload = &jpeg_data[i + 2..i + seg_len];
            if payload.len() >= 12 && payload.starts_with(b"Adobe") {
                return Some(payload[11]);
            }
        }
        if marker == 0xDA {
            break;
        }
        i += seg_len;
    }
    None
}

/// Decode a JBIG2-compressed PDF image stream into raw grayscale pixels.
#[cfg(feature = "rendering")]
pub(super) fn decode_jbig2_image(
    xobject: &crate::object::Object,
    obj_ref: Option<ObjectRef>,
    dict: &std::collections::HashMap<String, crate::object::Object>,
    doc: Option<&crate::document::PdfDocument>,
    width: u32,
    height: u32,
) -> Result<ImageData> {
    // The Jbig2Decoder in src/decoders/jbig2.rs is a pass-through: it returns
    // the raw compressed bitstream unchanged, which is exactly what hayro-jbig2
    // needs as input.
    let jbig2_bytes: Vec<u8> = if let (Some(d), Some(ref_id)) = (doc.as_ref(), obj_ref) {
        d.decode_stream_with_encryption(xobject, ref_id)?
    } else {
        xobject.decode_stream_data()?
    };

    // Load optional JBIG2Globals (shared symbol dictionaries referenced by multiple
    // embedded JBIG2 streams in the same PDF).
    let globals: Option<Vec<u8>> = (|| -> Option<Vec<u8>> {
        let dp = dict.get("DecodeParms")?.as_dict()?;
        let globals_ref = dp.get("JBIG2Globals")?.as_reference()?;
        let d = doc.as_ref()?;
        let globals_obj = d.load_object(globals_ref).ok()?;
        d.decode_stream_with_encryption(&globals_obj, globals_ref)
            .ok()
    })();

    let image = hayro_jbig2::Image::new_embedded(&jbig2_bytes, globals.as_deref())
        .map_err(|e| Error::Image(format!("JBIG2 decode error: {e}")))?;

    struct PixelCollector {
        pixels: Vec<u8>,
        row_buf: Vec<u8>,
    }

    impl hayro_jbig2::Decoder for PixelCollector {
        fn push_pixel(&mut self, black: bool) {
            self.row_buf.push(if black { 0 } else { 255 });
        }

        // chunk_count is the number of 8-pixel groups, not individual pixels.
        fn push_pixel_chunk(&mut self, black: bool, chunk_count: u32) {
            let v = if black { 0u8 } else { 255u8 };
            let n = chunk_count as usize * 8;
            self.row_buf.extend(std::iter::repeat_n(v, n));
        }

        fn next_line(&mut self) {
            self.pixels.append(&mut self.row_buf);
        }
    }

    let mut collector = PixelCollector {
        pixels: Vec::with_capacity((width * height) as usize),
        row_buf: Vec::with_capacity(width as usize),
    };

    image
        .decode(&mut collector)
        .map_err(|e| Error::Image(format!("JBIG2 pixel decode error: {e}")))?;

    Ok(ImageData::Raw {
        pixels: collector.pixels,
        format: PixelFormat::Grayscale,
    })
}

#[cfg(not(feature = "rendering"))]
pub(super) fn decode_jbig2_image(
    _xobject: &crate::object::Object,
    _obj_ref: Option<ObjectRef>,
    _dict: &std::collections::HashMap<String, crate::object::Object>,
    _doc: Option<&crate::document::PdfDocument>,
    _width: u32,
    _height: u32,
) -> Result<ImageData> {
    Err(Error::UnsupportedFilter(
        "JBIG2Decode — rebuild with the `rendering` feature to decode".to_string(),
    ))
}

/// Decode a JPEG 2000 (`/JPXDecode`) image stream into raw interleaved samples.
///
/// `JpxDecoder` is a pass-through, so `decode_stream_*` yields the raw JPEG 2000
/// codestream, which OpenJPEG (`decoders::jpx::decode_jpx`) decodes to 8-bit
/// component-interleaved samples.
#[cfg(feature = "jpeg2000")]
pub(super) fn decode_jpx_image(
    xobject: &crate::object::Object,
    obj_ref: Option<ObjectRef>,
    doc: Option<&crate::document::PdfDocument>,
    color_space: &ColorSpace,
) -> Result<ImageData> {
    let codestream: Vec<u8> = if let (Some(d), Some(ref_id)) = (doc.as_ref(), obj_ref) {
        d.decode_stream_with_encryption(xobject, ref_id)?
    } else {
        xobject.decode_stream_data()?
    };

    let img = crate::decoders::jpx::decode_jpx(&codestream)?;

    // The decoded sample layout is fixed by the codestream's component count
    // (ISO 32000-1 §7.4.9: a JPX stream carries its own colour space, which
    // agrees with the component count). The XObject's /ColorSpace is reserved
    // for future disambiguation (e.g. SMask/alpha handling).
    let _ = color_space;
    let format = match img.num_components {
        1 => PixelFormat::Grayscale,
        3 => PixelFormat::RGB,
        4 => PixelFormat::CMYK,
        n => {
            return Err(Error::UnsupportedFilter(format!(
                "JPXDecode: unsupported JPEG 2000 component count {n}"
            )))
        }
    };

    Ok(ImageData::Raw {
        pixels: img.samples,
        format,
    })
}

#[cfg(not(feature = "jpeg2000"))]
pub(super) fn decode_jpx_image(
    _xobject: &crate::object::Object,
    _obj_ref: Option<ObjectRef>,
    _doc: Option<&crate::document::PdfDocument>,
    _color_space: &ColorSpace,
) -> Result<ImageData> {
    Err(Error::UnsupportedFilter(
        "JPXDecode (JPEG 2000) — rebuild with the `jpeg2000` feature to decode".to_string(),
    ))
}
