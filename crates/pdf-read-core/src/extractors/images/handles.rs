use super::*;

// ── Phase 1 / Phase 2 split: enumerate-then-materialize image API ─────────────

/// A PDF stream filter as stored in the `/Filter` key of an image XObject.
///
/// Knowing the filter chain lets callers decide whether to decode (e.g. skip
/// decompression for JPEG re-embed pipelines that only need `raw_compressed_bytes`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub enum PdfFilter {
    /// JPEG (DCTDecode) — compressed bytes are a valid JPEG file.
    DCTDecode,
    /// JPEG 2000 (JPXDecode).
    JPXDecode,
    /// Deflate/zlib (FlateDecode).
    FlateDecode,
    /// LZW compression (LZWDecode).
    LZWDecode,
    /// CCITT Group 3/4 fax (CCITTFaxDecode).
    CCITTFaxDecode,
    /// JBIG2 bi-level compression.
    JBIG2Decode,
    /// ASCII hex encoding (ASCIIHexDecode).
    ASCIIHexDecode,
    /// ASCII base-85 encoding (ASCII85Decode).
    ASCII85Decode,
    /// Run-length encoding (RunLengthDecode).
    RunLengthDecode,
    /// Crypt filter (used with encrypted streams).
    Crypt,
    /// Any filter not listed above; carries the raw PDF name.
    Other(String),
}

impl PdfFilter {
    /// Map a PDF filter name (or its abbreviated form) to a `PdfFilter` variant.
    pub fn from_name(name: &str) -> Self {
        match name {
            "DCTDecode" | "DCT" => PdfFilter::DCTDecode,
            "JPXDecode" => PdfFilter::JPXDecode,
            "FlateDecode" | "Fl" => PdfFilter::FlateDecode,
            "LZWDecode" | "LZW" => PdfFilter::LZWDecode,
            "CCITTFaxDecode" | "CCF" => PdfFilter::CCITTFaxDecode,
            "JBIG2Decode" => PdfFilter::JBIG2Decode,
            "ASCIIHexDecode" | "AHx" => PdfFilter::ASCIIHexDecode,
            "ASCII85Decode" | "A85" => PdfFilter::ASCII85Decode,
            "RunLengthDecode" | "RL" => PdfFilter::RunLengthDecode,
            "Crypt" => PdfFilter::Crypt,
            other => PdfFilter::Other(other.to_string()),
        }
    }
}

/// Parses the `/Filter` entry of an image dictionary into a `Vec<PdfFilter>`.
///
/// The spec allows either a single name (`/DCTDecode`) or an array of names
/// (`[/ASCII85Decode /FlateDecode]`).
pub(crate) fn parse_filter_chain(
    dict: &std::collections::HashMap<String, crate::object::Object>,
) -> Vec<PdfFilter> {
    use crate::object::Object;
    match dict.get("Filter") {
        Some(Object::Name(n)) => vec![PdfFilter::from_name(n)],
        Some(Object::Array(arr)) => arr
            .iter()
            .filter_map(|o| o.as_name())
            .map(PdfFilter::from_name)
            .collect(),
        _ => vec![],
    }
}

/// Internal image source stored inside a [`PdfImageHandle`].
#[derive(Clone)]
enum PdfImageSource {
    /// Indirect Image XObject reference; loaded on demand.
    XObject(ObjectRef),
    /// Inline image: pre-built `Object::Stream` plus the raw compressed bytes.
    Inline {
        /// Synthetic `Object::Stream` built from the inline dict + data —
        /// ready to pass directly to `extract_image_from_xobject`.
        stream_object: crate::object::Object,
        /// Raw compressed bytes as they appeared between `ID` and `EI`.
        /// Stored as `bytes::Bytes` (cheaply cloneable, refcounted) so that
        /// the same allocation can be shared with the Stream data field
        /// without duplicating a potentially large JPEG/JBIG2/etc payload.
        compressed_bytes: bytes::Bytes,
    },
}

/// A lightweight handle to a PDF image that has **not** been decoded yet.
///
/// Created by [`crate::PdfDocument::page_image_handles`], which walks the page content
/// stream and reads XObject dictionary metadata without decompressing any stream.
/// Callers can inspect the metadata fields to decide which images to materialise,
/// then call [`decode`](PdfImageHandle::decode) or
/// [`raw_compressed_bytes`](PdfImageHandle::raw_compressed_bytes) only on those
/// they actually need.
///
/// # Example
///
/// ```no_run
/// # use pdf_oxide::PdfDocument;
/// # let bytes = std::fs::read("page.pdf").unwrap();
/// let doc = PdfDocument::from_bytes(bytes).unwrap();
/// // Phase 1: enumerate without decompression
/// let handles = doc.page_image_handles(0).unwrap();
/// // Phase 2: decode only images larger than a thumbnail
/// let images: Vec<_> = handles
///     .into_iter()
///     .filter(|h| h.width >= 200 && h.height >= 200)
///     .map(|h| h.decode())
///     .collect::<Result<_, _>>()
///     .unwrap();
/// ```
#[derive(Clone)]
#[non_exhaustive]
pub struct PdfImageHandle<'doc> {
    /// Image width in pixels (from XObject `/Width`).
    pub width: u32,
    /// Image height in pixels (from XObject `/Height`).
    pub height: u32,
    /// Colour space (from XObject `/ColorSpace`).
    pub color_space: ColorSpace,
    /// Bits per component (from XObject `/BitsPerComponent`).
    pub bits_per_component: u8,
    /// Compressed stream length in bytes (from XObject `/Length`).
    ///
    /// For inline images this is `data.len()` as stored between `ID` and `EI`.
    pub byte_size_compressed: u64,
    /// Ordered list of filters applied to the stream (outermost first).
    pub filter_chain: Vec<PdfFilter>,
    /// `true` if the image is an inline image (embedded in the content stream).
    pub is_inline: bool,
    /// Zero-based index of this image among all images painted on the page,
    /// in content-stream paint order.
    pub paint_order: usize,
    /// Axis-aligned bounding box of this image in PDF user space, computed
    /// during Phase 1 by applying the current transformation matrix to the
    /// unit rectangle `[0,0,1,1]`.
    pub bbox: crate::geometry::Rect,
    /// Rotation angle in degrees (0, 90, 180, or 270), derived from the CTM
    /// during Phase 1.
    pub rotation_degrees: f32,

    // Internal fields
    ctm: crate::content::Matrix,
    doc: &'doc crate::document::PdfDocument,
    source: PdfImageSource,
    /// Active resource `/ColorSpace` subdictionary (name → resolved Object) for
    /// this image's scope, so `decode()` can resolve a resource-name
    /// `/ColorSpace` (e.g. `/CS0`) the same way the renderer does. Empty when the
    /// image's colour space needs no resource lookup (the common case), which
    /// preserves the original `color_space_map=None` decode path.
    color_space_resources: std::collections::HashMap<String, crate::object::Object>,
    /// For an Indexed image (`[/Indexed base hival lookup]`, §8.6.6.3), the
    /// resolved de-indexed *base* colour space; `None` for non-Indexed images.
    indexed_base: Option<ColorSpace>,
}

impl<'doc> PdfImageHandle<'doc> {
    /// Decode this image into a [`PdfImage`].
    ///
    /// This is the expensive operation: it decompresses the image stream,
    /// decodes pixels, and applies colour-space conversions as needed.
    ///
    /// Takes `&self` so a single handle supports a two-phase inspect → raw →
    /// decode flow ([`raw_compressed_bytes`](Self::raw_compressed_bytes) then
    /// `decode`) without re-enumerating the page.
    pub fn decode(&self) -> Result<PdfImage> {
        use crate::extractors::extract_image_from_xobject;

        let xobject_for_extract;
        let (obj, obj_ref) = match &self.source {
            PdfImageSource::XObject(obj_ref) => {
                xobject_for_extract = self.doc.load_object(*obj_ref)?;
                (&xobject_for_extract, Some(*obj_ref))
            }
            PdfImageSource::Inline { stream_object, .. } => (stream_object, None),
        };

        // Pass the active resource ColorSpace map so a resource-name
        // `/ColorSpace` (e.g. `/CS0`) resolves the same way the renderer does.
        // `None` when empty preserves the original decode path for the common
        // case where the image's colour space needs no resource lookup.
        let cs_map = if self.color_space_resources.is_empty() {
            None
        } else {
            Some(&self.color_space_resources)
        };
        let mut image = extract_image_from_xobject(Some(self.doc), obj, obj_ref, cs_map)?;

        // Use pre-computed bbox and rotation from Phase 1 — no need to call
        // back into document.rs helpers here.
        image.set_bbox(self.bbox);
        image.set_matrix([
            self.ctm.a, self.ctm.b, self.ctm.c, self.ctm.d, self.ctm.e, self.ctm.f,
        ]);
        image.set_rotation_degrees(self.rotation_degrees as i32);

        Ok(image)
    }

    /// Return the raw compressed bytes exactly as stored in the PDF stream,
    /// **without** decompressing them.
    ///
    /// For JPEG images (`filter_chain == [DCTDecode]`) these bytes form a valid
    /// JPEG file and can be written directly to disk or forwarded to a downstream
    /// pipeline without recompression.
    ///
    /// Takes `&self` so it can be combined with [`decode`](Self::decode) on the
    /// same handle (inspect → raw → decode) without re-enumerating the page.
    pub fn raw_compressed_bytes(&self) -> Result<Vec<u8>> {
        match &self.source {
            PdfImageSource::XObject(obj_ref) => {
                let obj = self.doc.load_object(*obj_ref)?;
                match obj {
                    crate::object::Object::Stream { data, .. } => Ok(data.to_vec()),
                    _ => Err(crate::error::Error::Image(
                        "XObject is not a stream".to_string(),
                    )),
                }
            }
            PdfImageSource::Inline {
                compressed_bytes, ..
            } => Ok(compressed_bytes.to_vec()),
        }
    }

    /// For an Indexed image, the de-indexed *base* colour space.
    ///
    /// When [`color_space`](Self::color_space) is [`ColorSpace::Indexed`] the
    /// image samples are single palette indices (`components() == 1`) into an
    /// `[/Indexed base hival lookup]` array (§8.6.6.3); this returns the resolved
    /// `base` colour space — the space in which the de-indexed output pixels are
    /// expressed. Returns `None` for every non-Indexed image (and only if an
    /// Indexed `base` could not be parsed at all).
    pub fn indexed_base(&self) -> Option<ColorSpace> {
        self.indexed_base
    }
}

impl std::fmt::Debug for PdfImageHandle<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PdfImageHandle")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("color_space", &self.color_space)
            .field("bits_per_component", &self.bits_per_component)
            .field("byte_size_compressed", &self.byte_size_compressed)
            .field("filter_chain", &self.filter_chain)
            .field("is_inline", &self.is_inline)
            .field("paint_order", &self.paint_order)
            .field("bbox", &self.bbox)
            .field("rotation_degrees", &self.rotation_degrees)
            .field("color_space_resources", &self.color_space_resources)
            .field("indexed_base", &self.indexed_base)
            .finish_non_exhaustive()
    }
}

/// Derive a rotation angle in degrees from a transformation matrix.
///
/// Computes `atan2(b, a)` and rounds to the nearest integer degree.
fn matrix_to_rotation(m: crate::content::Matrix) -> f32 {
    let angle_rad = m.b.atan2(m.a);
    let angle_deg = angle_rad.to_degrees();
    let normalized = angle_deg % 360.0;
    if normalized < 0.0 {
        normalized + 360.0
    } else {
        normalized
    }
}

/// Transform an axis-aligned bounding rectangle by a CTM.
///
/// Transforms all four corners and returns the axis-aligned bounding box of the
/// result, which correctly handles rotation, shear, and negative scaling.
fn transform_bbox_with_ctm(
    rect: &crate::geometry::Rect,
    ctm: crate::content::Matrix,
) -> crate::geometry::Rect {
    let x0 = rect.x;
    let y0 = rect.y;
    let x1 = rect.x + rect.width;
    let y1 = rect.y + rect.height;

    let tx0 = ctm.a * x0 + ctm.c * y0 + ctm.e;
    let ty0 = ctm.b * x0 + ctm.d * y0 + ctm.f;

    let tx1 = ctm.a * x1 + ctm.c * y0 + ctm.e;
    let ty1 = ctm.b * x1 + ctm.d * y0 + ctm.f;

    let tx2 = ctm.a * x0 + ctm.c * y1 + ctm.e;
    let ty2 = ctm.b * x0 + ctm.d * y1 + ctm.f;

    let tx3 = ctm.a * x1 + ctm.c * y1 + ctm.e;
    let ty3 = ctm.b * x1 + ctm.d * y1 + ctm.f;

    let min_x = tx0.min(tx1).min(tx2).min(tx3);
    let max_x = tx0.max(tx1).max(tx2).max(tx3);
    let min_y = ty0.min(ty1).min(ty2).min(ty3);
    let max_y = ty0.max(ty1).max(ty2).max(ty3);

    crate::geometry::Rect {
        x: min_x,
        y: min_y,
        width: max_x - min_x,
        height: max_y - min_y,
    }
}

/// Resolve a handle's reported colour space from a raw `/ColorSpace` entry.
///
/// Shared by the XObject and inline handle builders so the two paths cannot
/// drift. Returns the `(color_space, indexed_base)` pair stored on the handle:
///
/// - **Resource-name resolution.** If `entry` is an `Object::Name` that is *not*
///   one of the standard device spaces (`DeviceGray`/`DeviceRGB`/`DeviceCMYK`/
///   `Pattern`, which "always identify the corresponding colour spaces directly"
///   and "never refer to resources", §8.6.3/§8.9.7), it is looked up in
///   `color_space_resources` (the active `/Resources/ColorSpace` subdictionary),
///   mirroring the renderer / `extract_image_from_xobject`.
/// - **Indirect-ref hop.** If the entry (or looked-up value) is a `Reference`,
///   one `load_object` hop is resolved via `doc`.
/// - **Indexed (§8.6.6.3).** When the resolved object is an
///   `[/Indexed base hival lookup]` array, returns `(ColorSpace::Indexed,
///   Some(base))` with the de-indexed `base` resolved (including an indirect
///   `base` ref and inner refs, e.g. `[/ICCBased <stream_ref>]`), reusing the
///   same base-resolution logic as `resolve_indexed_palette`. If the base cannot
///   be parsed, returns `(ColorSpace::Indexed, None)`.
/// - Otherwise returns `(parse_color_space(resolved)?, None)`, falling back to
///   `DeviceRGB` on parse failure.
pub(super) fn resolve_color_space_for_handle(
    entry: &crate::object::Object,
    color_space_resources: &std::collections::HashMap<String, crate::object::Object>,
    doc: Option<&crate::document::PdfDocument>,
) -> (ColorSpace, Option<ColorSpace>) {
    use crate::object::Object;

    // (a) Resource-name → resolved Object (skip standard device names, which
    // always identify their space directly and never refer to resources).
    let after_name = match entry {
        Object::Name(name)
            if !matches!(
                name.as_str(),
                "DeviceGray" | "DeviceRGB" | "DeviceCMYK" | "Pattern"
            ) =>
        {
            color_space_resources
                .get(name)
                .cloned()
                .unwrap_or_else(|| entry.clone())
        }
        _ => entry.clone(),
    };

    // (b) Resolve one indirect-ref hop.
    let resolved = match (after_name.as_reference(), doc) {
        (Some(r), Some(d)) => d.load_object(r).unwrap_or(after_name),
        _ => after_name,
    };

    // (c) `[/Indexed base hival lookup]` (§8.6.6.3): report Indexed + base.
    if let Object::Array(arr) = &resolved {
        if arr.first().and_then(|o| o.as_name()) == Some("Indexed") && arr.len() >= 2 {
            // Resolve the base object, mirroring resolve_indexed_palette: an
            // indirect base ref, plus inner refs of an array base such as
            // [/ICCBased <stream_ref>] so parse_color_space can read /N.
            let base_obj = if let Some(d) = doc {
                let outer = if let Some(r) = arr[1].as_reference() {
                    d.load_object(r).unwrap_or_else(|_| arr[1].clone())
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
            let base = parse_color_space(&base_obj).ok();
            return (ColorSpace::Indexed, base);
        }
    }

    // (d) Non-Indexed: parse directly, fall back to DeviceRGB.
    match parse_color_space(&resolved) {
        Ok(cs) => (cs, None),
        Err(_) => (ColorSpace::DeviceRGB, None),
    }
}

/// Build a `PdfImageHandle` from an Image XObject dictionary entry.
///
/// Returns `None` if the XObject reference cannot be resolved or the dict lacks
/// required fields (`Width`, `Height`), or if those fields contain non-positive
/// values.
pub(crate) fn image_handle_from_xobject<'doc>(
    doc: &'doc crate::document::PdfDocument,
    obj_ref: ObjectRef,
    xobject_dict: &std::collections::HashMap<String, crate::object::Object>,
    ctm: crate::content::Matrix,
    paint_order: usize,
    color_space_resources: &std::collections::HashMap<String, crate::object::Object>,
) -> Option<PdfImageHandle<'doc>> {
    let w = xobject_dict
        .get("Width")
        .and_then(|o| o.as_integer())
        .filter(|&n| n > 0)
        .map(|n| n as u32)?;
    let h = xobject_dict
        .get("Height")
        .and_then(|o| o.as_integer())
        .filter(|&n| n > 0)
        .map(|n| n as u32)?;
    let bpc = xobject_dict
        .get("BitsPerComponent")
        .and_then(|o| o.as_integer())
        .unwrap_or(8) as u8;
    let byte_size = xobject_dict
        .get("Length")
        .and_then(|o| o.as_integer())
        .filter(|&n| n >= 0)
        .map(|n| n as u64)
        .unwrap_or(0);
    let filter_chain = parse_filter_chain(xobject_dict);

    // Resolve the reported colour space via the shared helper: resource-name →
    // map entry, one indirect-ref hop, and `[/Indexed base ...]` (§8.6.6.3) →
    // `Indexed` + de-indexed base. Default to DeviceRGB when `/ColorSpace` is
    // absent.
    let (color_space, indexed_base) = match xobject_dict.get("ColorSpace") {
        Some(entry) => resolve_color_space_for_handle(entry, color_space_resources, Some(doc)),
        None => (ColorSpace::DeviceRGB, None),
    };

    // Compute bbox and rotation in Phase 1 while the CTM is in scope.
    let unit_rect = crate::geometry::Rect::new(0.0, 0.0, 1.0, 1.0);
    let bbox = transform_bbox_with_ctm(&unit_rect, ctm);
    let rotation_degrees = matrix_to_rotation(ctm);

    Some(PdfImageHandle {
        width: w,
        height: h,
        color_space,
        bits_per_component: bpc,
        byte_size_compressed: byte_size,
        filter_chain,
        is_inline: false,
        paint_order,
        bbox,
        rotation_degrees,
        ctm,
        doc,
        source: PdfImageSource::XObject(obj_ref),
        color_space_resources: color_space_resources.clone(),
        indexed_base,
    })
}

/// Build a `PdfImageHandle` from an inline image (`BI`/`ID`/`EI` sequence).
pub(crate) fn image_handle_from_inline<'doc>(
    doc: &'doc crate::document::PdfDocument,
    dict: &std::collections::HashMap<String, crate::object::Object>,
    data: Vec<u8>,
    ctm: crate::content::Matrix,
    paint_order: usize,
    color_space_resources: &std::collections::HashMap<String, crate::object::Object>,
) -> Option<PdfImageHandle<'doc>> {
    use crate::object::Object;

    // Inline image dicts use abbreviated keys; expand them.
    let expanded = crate::extractors::expand_inline_image_dict(dict.clone());

    let w = expanded
        .get("Width")
        .and_then(|o| o.as_integer())
        .filter(|&n| n > 0)
        .map(|n| n as u32)?;
    let h = expanded
        .get("Height")
        .and_then(|o| o.as_integer())
        .filter(|&n| n > 0)
        .map(|n| n as u32)?;
    let bpc = expanded
        .get("BitsPerComponent")
        .and_then(|o| o.as_integer())
        .unwrap_or(8) as u8;
    let byte_size = data.len() as u64;
    let filter_chain = parse_filter_chain(&expanded);

    // Resolve the reported colour space via the same shared helper as the
    // XObject path so the two agree. For inline images a resource-name
    // `/ColorSpace` into `/Resources/ColorSpace` is explicitly legal (§8.9.7),
    // and `[/Indexed base ...]` (§8.6.6.3) reports `Indexed` + de-indexed base.
    let (color_space, indexed_base) = match expanded.get("ColorSpace") {
        Some(entry) => resolve_color_space_for_handle(entry, color_space_resources, Some(doc)),
        None => (ColorSpace::DeviceRGB, None),
    };

    // Compute bbox and rotation in Phase 1 while the CTM is in scope.
    let unit_rect = crate::geometry::Rect::new(0.0, 0.0, 1.0, 1.0);
    let bbox = transform_bbox_with_ctm(&unit_rect, ctm);
    let rotation_degrees = matrix_to_rotation(ctm);

    // Build a synthetic Object::Stream so decode() can call extract_image_from_xobject.
    // Share a single Bytes allocation between the Stream (for decode) and the
    // handle (for raw_compressed_bytes). Bytes is refcounted, so this avoids
    // duplicating potentially large image payloads (e.g. 10 MB JPEG → 20 MB RSS).
    let mut stream_dict = expanded;
    stream_dict.insert("Subtype".to_string(), Object::Name("Image".to_string()));
    let compressed_bytes = bytes::Bytes::from(data);
    let stream_object = Object::Stream {
        dict: stream_dict,
        data: compressed_bytes.clone(),
    };

    Some(PdfImageHandle {
        width: w,
        height: h,
        color_space,
        bits_per_component: bpc,
        byte_size_compressed: byte_size,
        filter_chain,
        is_inline: true,
        paint_order,
        bbox,
        rotation_degrees,
        ctm,
        doc,
        source: PdfImageSource::Inline {
            stream_object,
            compressed_bytes,
        },
        color_space_resources: color_space_resources.clone(),
        indexed_base,
    })
}
