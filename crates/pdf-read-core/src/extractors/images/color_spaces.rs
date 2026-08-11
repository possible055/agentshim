use super::*;

/// Outcome of resolving an `[/Indexed base hival lookup]` colour space:
/// the palette in the base's pixel format, plus the base's ICC profile
/// when the base is `ICCBased`.
pub(crate) struct IndexedResolution {
    pub base_fmt: PixelFormat,
    pub palette: Vec<u8>,
    /// `None` for device-dependent bases or bases we already folded
    /// colourimetrically (e.g. Lab, whose palette is rewritten to RGB
    /// before being returned).
    pub base_profile: Option<std::sync::Arc<crate::color::IccProfile>>,
}

/// Resolve an Indexed color space's base color space and palette lookup bytes.
///
/// PDF Indexed color spaces are `[/Indexed base hival lookup]` where `lookup`
/// is either a byte string or a stream of `(hival + 1) * N` bytes (N = number
/// of components in the base color space).
pub(super) fn resolve_indexed_palette(
    doc: Option<&crate::document::PdfDocument>,
    cs_obj: &crate::object::Object,
) -> Result<Option<IndexedResolution>> {
    use crate::object::Object;

    let Object::Array(arr) = cs_obj else {
        return Ok(None);
    };
    if arr.len() < 4 {
        return Ok(None);
    }

    // Resolve the base color-space object. When it's an array like
    // [/ICCBased <stream_ref>], resolve inner references so
    // parse_color_space can read /N from the ICC stream dict.
    let base_obj = if let Some(d) = doc {
        let outer = if let Some(r) = arr[1].as_reference() {
            d.load_object(r)?
        } else {
            arr[1].clone()
        };
        if let Object::Array(mut inner) = outer {
            for item in inner.iter_mut() {
                if let Some(r) = item.as_reference() {
                    if let Ok(resolved) = d.load_object(r) {
                        *item = resolved;
                    }
                }
            }
            Object::Array(inner)
        } else {
            outer
        }
    } else {
        arr[1].clone()
    };
    let base_cs = parse_color_space(&base_obj)?;
    let base_fmt = color_space_to_pixel_format(&base_cs);
    let n = base_fmt.bytes_per_pixel();

    // When the base is `/ICCBased`, capture the profile bytes so the
    // extractor can later hand them to a CMM. Parse failures reduce to
    // `None` — the decoder then falls back to §10.3.5 CMYK→RGB math as
    // if no profile were present.
    let base_profile = if matches!(base_cs, ColorSpace::ICCBased(_)) {
        resolve_icc_profile_from_obj(doc, &base_obj)
    } else {
        None
    };

    // hival bounds the valid index range. Resolve via indirect reference if
    // needed; treat invalid / missing values as "unknown" and skip truncation.
    let hival_obj = if let Some(d) = doc {
        if let Some(r) = arr[2].as_reference() {
            d.load_object(r)?
        } else {
            arr[2].clone()
        }
    } else {
        arr[2].clone()
    };
    let hival: Option<usize> = hival_obj.as_integer().and_then(|i| {
        if (0..=255).contains(&i) {
            Some(i as usize)
        } else {
            None
        }
    });

    let lookup_obj = if let Some(d) = doc {
        if let Some(r) = arr[3].as_reference() {
            d.load_object(r)?
        } else {
            arr[3].clone()
        }
    } else {
        arr[3].clone()
    };
    let mut palette_bytes = match &lookup_obj {
        Object::String(s) => s.clone(),
        Object::Stream { .. } => lookup_obj.decode_stream_data()?,
        _ => return Ok(None),
    };
    if palette_bytes.is_empty() {
        return Ok(None);
    }

    // Truncate palette to the logical length implied by hival so that indices
    // greater than hival fall into the out-of-range branch of the expander.
    // Per PDF 32000-1:2008 §8.6.6.3 the lookup is exactly (hival + 1) * N bytes;
    // anything beyond that is stray data that must not be mapped to pixels.
    if let Some(h) = hival {
        let expected = (h + 1).saturating_mul(n);
        if expected > 0 && palette_bytes.len() > expected {
            palette_bytes.truncate(expected);
        }
    }

    // Device-independent colour-space palettes must be converted to
    // RGB before being handed to the expander, which assumes palette
    // bytes are already in the output colour space. Without this step
    // Lab triples are mis-interpreted as raw RGB and render with
    // perceptually wrong colours.
    if matches!(base_cs, ColorSpace::Lab) {
        let white = extract_lab_whitepoint(&base_obj);
        let rgb_palette = lab_palette_to_rgb(&palette_bytes, white);
        // Lab palettes are now RGB; no base ICC profile to carry through.
        return Ok(Some(IndexedResolution {
            base_fmt: PixelFormat::RGB,
            palette: rgb_palette,
            base_profile: None,
        }));
    }

    Ok(Some(IndexedResolution {
        base_fmt,
        palette: palette_bytes,
        base_profile,
    }))
}

/// Reduce a big-endian 16-bit colour sample (`hi`, `lo` bytes) to 8 bits,
/// rounding `v * 255 / 65535` to the nearest value. Preferred over the crude
/// high-byte drop (`v >> 8`), which floors and biases every sample downward by
/// up to ~1 LSB. `v == 0xFFFF` maps to `255` and `v == 0` to `0` exactly.
#[inline]
pub(super) fn reduce_16_to_8(hi: u8, lo: u8) -> u8 {
    let v = u16::from_be_bytes([hi, lo]) as u32;
    ((v * 255 + 32_767) / 65_535) as u8
}

/// Expand packed Indexed image indices into RGB bytes using the palette.
///
/// Supports 1, 2, 4, and 8 bit-per-component index streams. Rows are padded
/// to byte boundaries per the PDF spec.
///
/// Returns `Err(Error::Image)` when the requested dimensions would require
/// more than `MAX_INDEXED_OUTPUT_BYTES` to decode, or when the `usize`
/// arithmetic on `width * height * channels` / `width * bpc` overflows,
/// or when the input `raw` buffer is too short to supply every row of the
/// requested height. This is an input-amplification guard for maliciously
/// crafted PDFs that pair tiny streams with extreme Indexed image
/// dimensions — see issue #324.
#[cfg(test)]
pub(super) fn expand_indexed_to_rgb(
    raw: &[u8],
    palette: &[u8],
    base_fmt: PixelFormat,
    width: u32,
    height: u32,
    bpc: u8,
) -> Result<Vec<u8>> {
    expand_indexed_to_rgb_with_transform(raw, palette, base_fmt, width, height, bpc, None)
}

/// Like [`expand_indexed_to_rgb`] but routes CMYK palette entries
/// through an ICC transform when one is supplied. Used during image
/// extraction when the base colour space is `/ICCBased` with N=4.
pub(super) fn expand_indexed_to_rgb_with_transform(
    raw: &[u8],
    palette: &[u8],
    base_fmt: PixelFormat,
    width: u32,
    height: u32,
    bpc: u8,
    transform: Option<&crate::color::Transform>,
) -> Result<Vec<u8>> {
    /// Hard cap on the decoded output buffer size (256 MiB). Legitimate
    /// Indexed images in real PDFs are several orders of magnitude below
    /// this — the cap only fires on pathological / adversarial inputs
    /// where `width * height` is billions of pixels.
    const MAX_INDEXED_OUTPUT_BYTES: usize = 256 * 1024 * 1024;

    let w = width as usize;
    let h = height as usize;
    let n = base_fmt.bytes_per_pixel();

    // ISO 32000-2 §8.9.5.1 mandates bpc ∈ {1, 2, 4, 8} for Indexed color
    // spaces. Anything else (0, 3, 5, 6, 7, 9, 12, 16, …) used to be
    // accepted silently — bpc=0 was coerced to 1 and any other value fell
    // through the `read_index` `_ => 0` arm, producing a solid palette-
    // entry-0 image with no error. Reject up front so malformed input is
    // surfaced instead of decoded into nonsense pixels.
    if !matches!(bpc, 1 | 2 | 4 | 8) {
        return Err(Error::Image(format!(
            "Indexed image has invalid /BitsPerComponent {bpc} \
             (PDF spec requires 1, 2, 4, or 8)"
        )));
    }

    // Checked arithmetic for `bytes_per_row = ceil(w * bpc / 8)`.
    let bytes_per_row = w
        .checked_mul(bpc as usize)
        .map(|v| v.div_ceil(8))
        .ok_or_else(|| {
            Error::Image(format!(
                "Indexed image row width overflow: {w} × {bpc} bpc exceeds usize"
            ))
        })?;

    // Checked arithmetic for `w * h * 3` (output always written as RGB).
    let output_bytes = w
        .checked_mul(h)
        .and_then(|v| v.checked_mul(3))
        .ok_or_else(|| {
            Error::Image(format!(
                "Indexed image output size overflow: {w} × {h} × 3 exceeds usize"
            ))
        })?;

    if output_bytes > MAX_INDEXED_OUTPUT_BYTES {
        return Err(Error::Image(format!(
            "Indexed image decode would produce {output_bytes} bytes, \
             exceeds guard limit of {MAX_INDEXED_OUTPUT_BYTES} bytes \
             (width={w}, height={h})"
        )));
    }

    // The decoded index stream must cover every row of the image.
    // Truncated streams used to get silently zero-padded, which lets a
    // malicious PDF pair a 10-byte stream with a 10 000 × 10 000 image
    // and force a ~300 MiB allocation filled with default palette entry
    // 0. Reject that shape up front.
    let required_bytes = bytes_per_row.checked_mul(h).ok_or_else(|| {
        Error::Image(format!(
            "Indexed image required-input size overflow: {bytes_per_row} × {h} exceeds usize"
        ))
    })?;
    if raw.len() < required_bytes {
        return Err(Error::Image(format!(
            "Indexed image index stream truncated: {} bytes available, \
             {} required ({} bytes/row × {} rows)",
            raw.len(),
            required_bytes,
            bytes_per_row,
            h
        )));
    }

    let mut out = Vec::with_capacity(output_bytes);

    let read_index = |row: &[u8], x: usize| -> usize {
        match bpc {
            8 => row.get(x).copied().unwrap_or(0) as usize,
            4 => {
                let byte_idx = x / 2;
                let b = row.get(byte_idx).copied().unwrap_or(0);
                if x.is_multiple_of(2) {
                    (b >> 4) as usize
                } else {
                    (b & 0x0F) as usize
                }
            }
            2 => {
                let byte_idx = x / 4;
                let b = row.get(byte_idx).copied().unwrap_or(0);
                let shift = 6 - (x % 4) * 2;
                ((b >> shift) & 0x03) as usize
            }
            1 => {
                let byte_idx = x / 8;
                let b = row.get(byte_idx).copied().unwrap_or(0);
                let shift = 7 - (x % 8);
                ((b >> shift) & 0x01) as usize
            }
            // Unreachable: bpc is validated to be in {1, 2, 4, 8} above
            // before the closure is called, so this arm only exists to
            // satisfy exhaustiveness on `u8`.
            _ => unreachable!("bpc validated to {{1,2,4,8}} before read_index"),
        }
    };

    for y in 0..h {
        let row_start = y * bytes_per_row;
        let row_end = (row_start + bytes_per_row).min(raw.len());
        let row: &[u8] = if row_start < raw.len() {
            &raw[row_start..row_end]
        } else {
            &[]
        };
        for x in 0..w {
            let idx = read_index(row, x);
            let off = idx * n;
            if off + n > palette.len() {
                out.extend_from_slice(&[0, 0, 0]);
                continue;
            }
            match base_fmt {
                PixelFormat::RGB => out.extend_from_slice(&palette[off..off + 3]),
                PixelFormat::Grayscale => {
                    let g = palette[off];
                    out.push(g);
                    out.push(g);
                    out.push(g);
                }
                PixelFormat::CMYK => {
                    let c = palette[off];
                    let m = palette[off + 1];
                    let y_c = palette[off + 2];
                    let k = palette[off + 3];
                    let [r, g, b] = if let Some(t) = transform {
                        t.convert_cmyk_pixel(c, m, y_c, k)
                    } else {
                        cmyk_pixel_to_rgb(c, m, y_c, k)
                    };
                    out.push(r);
                    out.push(g);
                    out.push(b);
                }
            }
        }
    }
    Ok(out)
}

/// Convert a single DeviceCMYK pixel to RGB.
///
/// Shared conversion math used by both bulk CMYK->RGB and Indexed palette
/// expansion so the two paths cannot drift apart.
///
/// Uses the PROCESS-INK conversion (`color::cmyk_to_rgb`, tetralinear over the
/// 16 measured ink corners of the CMYK cube), NOT the naive additive-then-clamp
/// `R = 1 - min(1, C+K)`. The additive form treats the inks as pure subtractive
/// primaries and so renders 100% K as `#000000` and 100% cyan as `#00FFFF`;
/// DeviceCMYK is a *device* space, so the colour is what the inks look like -
/// 100% K is `#231F20`, 100% cyan `#00ADEF`. This is the same conversion the
/// text/vector paths already use, so a DeviceCMYK image now matches the colour
/// of DeviceCMYK text and fills on the same page. For `/ICCBased` CMYK a real
/// CMM (qcms / lcms2) still takes precedence when a profile is available; this
/// is the no-profile fallback.
pub(crate) fn cmyk_pixel_to_rgb(c: u8, m: u8, y: u8, k: u8) -> [u8; 3] {
    let (r, g, b) = crate::color::cmyk_to_rgb(
        c as f32 / 255.0,
        m as f32 / 255.0,
        y as f32 / 255.0,
        k as f32 / 255.0,
    );
    [
        (r * 255.0).round() as u8,
        (g * 255.0).round() as u8,
        (b * 255.0).round() as u8,
    ]
}

/// Extract `/WhitePoint` from a Lab colour-space PDF object.
///
/// The object is `[/Lab << /WhitePoint [Xw Yw Zw] >>]`. Returns the
/// whitepoint as `[Xw, Yw, Zw]`, falling back to D65 if absent.
pub(super) fn extract_lab_whitepoint(cs_obj: &crate::object::Object) -> [f64; 3] {
    const D65: [f64; 3] = [0.9505, 1.0, 1.0890];
    let arr = match cs_obj {
        crate::object::Object::Array(a) => a,
        _ => return D65,
    };
    if arr.len() < 2 {
        return D65;
    }
    let dict = match &arr[1] {
        crate::object::Object::Dictionary(d) => d,
        _ => return D65,
    };
    let wp = match dict.get("WhitePoint") {
        Some(crate::object::Object::Array(a)) if a.len() >= 3 => a,
        _ => return D65,
    };
    let f = |obj: &crate::object::Object| -> Option<f64> {
        match obj {
            crate::object::Object::Real(v) => Some(*v),
            crate::object::Object::Integer(v) => Some(*v as f64),
            _ => None,
        }
    };
    match (f(&wp[0]), f(&wp[1]), f(&wp[2])) {
        (Some(x), Some(y), Some(z)) => [x, y, z],
        _ => D65,
    }
}

/// Convert a Lab-encoded palette to sRGB.
///
/// Each entry is 3 bytes: L* (byte 0), a* (byte 1), b* (byte 2).
/// Decoding per PDF 32000-1:2008 §8.6.5.4:
///   L* = byte_0 / 255.0 × 100.0
///   a* = byte_1 − 128.0   (default /Range [−128 127])
///   b* = byte_2 − 128.0
///
/// Then Lab → XYZ (whitepoint-relative) → sRGB with standard gamma.
pub(crate) fn lab_palette_to_rgb(palette: &[u8], white: [f64; 3]) -> Vec<u8> {
    let mut rgb = Vec::with_capacity(palette.len());
    for chunk in palette.chunks(3) {
        if chunk.len() < 3 {
            rgb.extend_from_slice(&[0, 0, 0]);
            continue;
        }
        let [r, g, b] = lab_pixel_to_rgb(chunk[0], chunk[1], chunk[2], white);
        rgb.push(r);
        rgb.push(g);
        rgb.push(b);
    }
    rgb
}

// NOTE: The XYZ→linear-sRGB matrix below assumes a D65 whitepoint. Lab CIEs
// whose `/WhitePoint` is non-D65 (D50 is common in print workflows) would
// strictly need chromatic adaptation (e.g., Bradford) from the source
// whitepoint to D65 before the sRGB matrix. We intentionally omit that for
// now — the vast majority of PDF `/Lab` spaces we encounter are D65 — but
// the caller's `white` is still used to scale `xw, yw, zw` so D65 and
// near-D65 whitepoints produce correct output. Non-D65 spaces will have a
// minor chromatic-adaptation error until this is revisited.
pub(super) fn lab_pixel_to_rgb(l_byte: u8, a_byte: u8, b_byte: u8, white: [f64; 3]) -> [u8; 3] {
    let l_star = l_byte as f64 / 255.0 * 100.0;
    let a_star = a_byte as f64 - 128.0;
    let b_star = b_byte as f64 - 128.0;

    let fy = (l_star + 16.0) / 116.0;
    let fx = a_star / 500.0 + fy;
    let fz = fy - b_star / 200.0;

    let [xw, yw, zw] = white;
    let x = xw * f_inv(fx);
    let y = yw * f_inv(fy);
    let z = zw * f_inv(fz);

    // XYZ → linear sRGB (D65 matrix, IEC 61966-2-1:1999)
    let r_lin = 3.2406254773 * x - 1.5372079722 * y - 0.4986285987 * z;
    let g_lin = -0.9689307147 * x + 1.8757560609 * y + 0.0415175580 * z;
    let b_lin = 0.0557101204 * x - 0.2040210506 * y + 1.0569959423 * z;

    [srgb_gamma(r_lin), srgb_gamma(g_lin), srgb_gamma(b_lin)]
}

fn f_inv(t: f64) -> f64 {
    const DELTA: f64 = 6.0 / 29.0;
    if t > DELTA {
        t * t * t
    } else {
        3.0 * DELTA * DELTA * (t - 4.0 / 29.0)
    }
}

fn srgb_gamma(lin: f64) -> u8 {
    let v = if lin <= 0.0031308 {
        12.92 * lin
    } else {
        1.055 * lin.powf(1.0 / 2.4) - 0.055
    };
    (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8
}
