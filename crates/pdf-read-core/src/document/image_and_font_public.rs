use super::parsing::*;
use super::preflight::*;
use super::*;

impl PdfDocument {
    /// Recursively extract images from a Form XObject.
    ///
    /// Uses a document-level cache: images are extracted once using only the Form's
    /// own Matrix, then cached. On subsequent references, cached images are cloned
    /// and the caller's CTM is applied to transform bboxes.
    pub(super) fn extract_images_from_form_xobject(
        &self,
        xobject_ref: ObjectRef,
        xobject: &Object,
        parent_resources: &Object,
        parent_ctm: crate::content::Matrix,
        xobject_stack: &mut Vec<ObjectRef>,
        filter: &ImageExtractFilter,
    ) -> Result<Vec<crate::extractors::PdfImage>> {
        use crate::content::parse_content_stream_images_only;
        use crate::content::Operator;

        // Cycle detection
        if xobject_stack.contains(&xobject_ref) || xobject_stack.len() >= 100 {
            return Ok(Vec::new());
        }

        // Check image result cache — images stored with Form's own Matrix only.
        // Scope the borrow to ensure it's dropped before potential recursion.
        {
            if let Some(cached_images) = self
                .form_xobject_images_cache
                .lock_or_recover()
                .get(&xobject_ref)
            {
                let images = cached_images
                    .iter()
                    .map(|img| {
                        let mut cloned = img.clone();
                        if let Some(rect) = cloned.bbox() {
                            cloned.set_bbox(self.transform_bbox_with_ctm(rect, parent_ctm));
                        }
                        cloned
                    })
                    .collect();
                return Ok(images);
            }
        }

        xobject_stack.push(xobject_ref);

        let xobj_dict = xobject.as_dict().ok_or_else(|| Error::ParseError {
            offset: 0,
            reason: "Form XObject is not a dictionary".to_string(),
        })?;

        // Get Form resources (with fallback to parent)
        let form_resources = if let Some(form_res) = xobj_dict.get("Resources") {
            if let Some(ref_obj) = form_res.as_reference() {
                self.load_object(ref_obj)?
            } else {
                form_res.clone()
            }
        } else {
            parent_resources.clone()
        };

        // Pre-resolve XObject dictionary for this form's resources
        let form_xobject_dict = if let Some(res_dict) = form_resources.as_dict() {
            if let Some(xobj_entry) = res_dict.get("XObject") {
                let resolved = if let Some(ref_obj) = xobj_entry.as_reference() {
                    self.load_object(ref_obj)?
                } else {
                    xobj_entry.clone()
                };
                resolved.as_dict().cloned()
            } else {
                None
            }
        } else {
            None
        };

        // Get Form transformation matrix (default to identity)
        let form_matrix = if let Some(matrix_obj) = xobj_dict.get("Matrix") {
            self.parse_matrix_from_object(matrix_obj)
                .unwrap_or_else(crate::content::Matrix::identity)
        } else {
            crate::content::Matrix::identity()
        };

        // Decode form stream — check cache first to avoid repeated decompression
        let cached_stream = self
            .xobject_stream_cache
            .lock_or_recover()
            .get(&xobject_ref)
            .cloned();
        let stream_data = if let Some(cached) = cached_stream {
            cached.as_ref().clone()
        } else {
            match self.decode_stream_with_encryption(xobject, xobject_ref) {
                Ok(data) => {
                    admit_xobject_stream(self, xobject_ref, &data);
                    data
                }
                Err(e) => {
                    log::warn!("Failed to decode Form XObject stream: {}, skipping", e);
                    xobject_stack.pop();
                    return Ok(Vec::new());
                }
            }
        };

        // Parse operators using fast image-only path (skips text operators)
        let operators = match parse_content_stream_images_only(&stream_data) {
            Ok(ops) => ops,
            Err(_) => {
                xobject_stack.pop();
                return Ok(Vec::new());
            }
        };

        // Extract using only the Form's own Matrix (no parent_ctm yet).
        // This allows caching the results and applying different parent CTMs later.
        let mut raw_images = Vec::new();
        let mut ctm_stack = vec![form_matrix];

        for op in operators {
            match op {
                Operator::SaveState => {
                    if let Some(current_ctm) = ctm_stack.last() {
                        ctm_stack.push(*current_ctm);
                    }
                }
                Operator::RestoreState => {
                    if ctm_stack.len() > 1 {
                        ctm_stack.pop();
                    }
                }
                Operator::Cm { a, b, c, d, e, f } => {
                    if let Some(current_ctm) = ctm_stack.last_mut() {
                        let matrix = crate::content::Matrix { a, b, c, d, e, f };
                        // PDF spec ISO 32000-1:2008 §8.3.4: cm concatenates as M_cm × CTM
                        *current_ctm = matrix.multiply(current_ctm);
                    }
                }

                Operator::Do { name } => {
                    if let Some(ref xobj_d) = form_xobject_dict {
                        let current_ctm = ctm_stack
                            .last()
                            .copied()
                            .unwrap_or_else(crate::content::Matrix::identity);
                        // For nested Do operators, pass identity as parent_ctm since
                        // we're building raw (un-transformed) images for caching
                        if let Ok(mut xobj_images) = self.extract_images_from_xobject_do(
                            &name,
                            xobj_d,
                            Some(&form_resources),
                            current_ctm,
                            xobject_stack,
                            filter,
                        ) {
                            raw_images.append(&mut xobj_images);
                        }
                    }
                }

                Operator::InlineImage { dict, data } => {
                    let current_ctm = ctm_stack
                        .last()
                        .copied()
                        .unwrap_or_else(crate::content::Matrix::identity);
                    if let Ok(image) = self.extract_image_from_inline(&dict, &data, current_ctm) {
                        raw_images.push(image);
                    }
                }

                _ => {}
            }
        }

        xobject_stack.pop();

        // Cache the raw images (with Form's own Matrix applied, but no parent CTM)
        self.form_xobject_images_cache
            .lock_or_recover()
            .insert(xobject_ref, raw_images.clone());

        // Apply parent_ctm to produce final images for this call
        let images = raw_images
            .into_iter()
            .map(|mut img| {
                if let Some(rect) = img.bbox() {
                    img.set_bbox(self.transform_bbox_with_ctm(rect, parent_ctm));
                }
                img
            })
            .collect();

        Ok(images)
    }

    /// Extract an inline image from the content stream.
    pub(super) fn extract_image_from_inline(
        &self,
        dict: &std::collections::HashMap<String, Object>,
        data: &[u8],
        ctm: crate::content::Matrix,
    ) -> Result<crate::extractors::PdfImage> {
        use crate::extractors::expand_inline_image_dict;

        // Expand abbreviated dictionary
        let expanded_dict = expand_inline_image_dict(dict.clone());

        // Build a temporary stream object from the dictionary and data
        let stream_obj = Object::Stream {
            dict: expanded_dict,
            data: bytes::Bytes::copy_from_slice(data),
        };

        // Use existing extraction logic
        let mut image =
            crate::extractors::extract_image_from_xobject(Some(self), &stream_obj, None, None)?;

        // In PDF, images are mapped from unit square (0,0 to 1,1) to the CTM.
        let unit_rect = crate::geometry::Rect::new(0.0, 0.0, 1.0, 1.0);
        let bbox = self.transform_bbox_with_ctm(&unit_rect, ctm);
        image.set_bbox(bbox);

        // Capture transformation matrix and rotation (v0.3.14)
        image.set_matrix([ctm.a, ctm.b, ctm.c, ctm.d, ctm.e, ctm.f]);
        image.set_rotation_degrees(Self::matrix_to_rotation(ctm));

        Ok(image)
    }

    /// Helper to derive rotation angle from transformation matrix.
    pub(super) fn matrix_to_rotation(m: crate::content::Matrix) -> i32 {
        // Compute angle from CTM components (atan2(b, a))
        let angle_rad = m.b.atan2(m.a);
        let angle_deg = (angle_rad.to_degrees().round() as i32) % 360;
        if angle_deg < 0 {
            angle_deg + 360
        } else {
            angle_deg
        }
    }

    /// Transform a bounding box using CTM.
    ///
    /// Transforms all four corners and computes the axis-aligned bounding box,
    /// which correctly handles rotation, shear, and negative scaling.
    pub(super) fn transform_bbox_with_ctm(
        &self,
        rect: &crate::geometry::Rect,
        ctm: crate::content::Matrix,
    ) -> crate::geometry::Rect {
        let x0 = rect.x;
        let y0 = rect.y;
        let x1 = rect.x + rect.width;
        let y1 = rect.y + rect.height;

        // Transform all four corners
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

    /// Parse a Matrix object from PDF.
    pub(super) fn parse_matrix_from_object(&self, obj: &Object) -> Option<crate::content::Matrix> {
        if let Some(array) = obj.as_array() {
            if array.len() >= 6 {
                let mut values = [0.0f32; 6];
                for (i, val) in array.iter().take(6).enumerate() {
                    let num = if let Some(f) = val.as_real() {
                        f as f32
                    } else {
                        let i_val = val.as_integer()?;
                        i_val as f32
                    };
                    values[i] = num;
                }

                return Some(crate::content::Matrix {
                    a: values[0],
                    b: values[1],
                    c: values[2],
                    d: values[3],
                    e: values[4],
                    f: values[5],
                });
            }
        }
        None
    }

    // ========================================================================
    // Debug/profiling helpers — thin pub wrappers over internal methods.
    // Used by examples/debug_katalog.rs to break extract_spans into phases.
    // ========================================================================

    /// Public wrapper for `get_page` (normally private).
    /// Exposed for profiling examples that need to time page tree lookup separately.
    pub fn get_page_for_debug(&self, page_index: usize) -> Result<Object> {
        self.get_page(page_index)
    }

    /// Public wrapper for `may_contain_text` (normally pub(crate)).
    /// Returns true if the content stream might contain text operators (BT or Do).
    pub fn may_contain_text_public(data: &[u8]) -> bool {
        Self::may_contain_text(data)
    }

    /// Public wrapper for `load_fonts` (normally pub(crate)).
    /// Loads font dictionaries from a resources object into a TextExtractor.
    pub fn load_fonts_public(
        &self,
        resources: &Object,
        extractor: &mut crate::extractors::TextExtractor<'_>,
    ) -> Result<()> {
        self.load_fonts(resources, extractor)
    }

    /// Per-page mapping of PDF font-resource names (e.g. `"F75"`) to their
    /// canonical face name (e.g. `"TeXGyreTermesX-Regular"`, with any
    /// subset-prefix `ABCDEF+` stripped).
    ///
    /// Used by the layout-preserving DOCX writer so each text span can be
    /// emitted with the actual face name in `<w:rFonts>` instead of a
    /// PDF-internal resource id. The vector is `pages × map`; `map[i]`
    /// covers all fonts referenced by page `i`'s Resources.
    pub fn page_font_face_lookups(&self) -> Result<Vec<std::collections::HashMap<String, String>>> {
        use std::collections::HashMap;
        let n = self.page_count()?;
        let mut out: Vec<HashMap<String, String>> = Vec::with_capacity(n);
        for page_idx in 0..n {
            let mut lookup: HashMap<String, String> = HashMap::new();
            // Inline get_page → Resources so this works without `rendering`.
            let resources = match self.get_page(page_idx) {
                Ok(page) => match page.as_dict() {
                    Some(d) => {
                        let r = d
                            .get("Resources")
                            .cloned()
                            .unwrap_or(Object::Dictionary(std::collections::HashMap::new()));
                        if let Some(rref) = r.as_reference() {
                            self.load_object(rref)
                                .unwrap_or(Object::Dictionary(std::collections::HashMap::new()))
                        } else {
                            r
                        }
                    }
                    None => {
                        out.push(lookup);
                        continue;
                    }
                },
                Err(_) => {
                    out.push(lookup);
                    continue;
                }
            };
            let mut extractor = crate::extractors::TextExtractor::new();
            if self.load_fonts_public(&resources, &mut extractor).is_ok() {
                for (resource_name, info) in extractor.get_font_set() {
                    let canonical = info
                        .base_font
                        .split_once('+')
                        .map(|(_, rest)| rest)
                        .unwrap_or(info.base_font.as_str())
                        .to_string();
                    lookup.insert(resource_name, canonical);
                }
            }
            out.push(lookup);
        }
        Ok(out)
    }

    /// Extract every embedded font program (TrueType / OpenType bytes) used
    /// anywhere in the document, deduplicated by `BaseFont` name.
    ///
    /// Walks every page's font dictionary, loads each font via the same path
    /// `extract_text` uses, and returns the unique set of fonts that have
    /// embedded `FontFile2`/`FontFile3` streams. The `String` is the base
    /// font name (with any subset prefix like `ABCDEF+` stripped) and the
    /// `Vec<u8>` is the raw font program — directly suitable for re-embedding
    /// into another container (DOCX `word/fonts/`, another PDF, etc.).
    ///
    /// Fonts without embedded data (standard 14, missing FontFile streams)
    /// are skipped — there's nothing to extract.
    pub fn extract_embedded_fonts(&self) -> Result<Vec<(String, Vec<u8>)>> {
        use std::collections::HashMap;
        let mut by_name: HashMap<String, Vec<u8>> = HashMap::new();

        let n = self.page_count()?;
        for page_idx in 0..n {
            // Inline get_page_resources so this works without `rendering`.
            let resources = match self.get_page(page_idx) {
                Ok(page) => match page.as_dict() {
                    Some(d) => {
                        let r = d
                            .get("Resources")
                            .cloned()
                            .unwrap_or(Object::Dictionary(std::collections::HashMap::new()));
                        if let Some(rref) = r.as_reference() {
                            self.load_object(rref).unwrap_or_else(|_| {
                                Object::Dictionary(std::collections::HashMap::new())
                            })
                        } else {
                            r
                        }
                    }
                    None => continue,
                },
                Err(_) => continue,
            };
            let mut extractor = crate::extractors::TextExtractor::new();
            if self.load_fonts_public(&resources, &mut extractor).is_err() {
                continue;
            }
            for (_resource_name, font_arc) in extractor.get_font_set() {
                let Some(data) = font_arc.embedded_font_data.as_ref() else {
                    continue;
                };
                if data.is_empty() {
                    continue;
                }
                // Subset-prefix stripping: PDF font subsets carry a 6-letter
                // prefix followed by `+`, e.g. `ABCDEF+Calibri-Bold`. The
                // prefix is meaningless to consumers — strip it for dedup.
                let base = font_arc.base_font.as_str();
                let canonical = base.split_once('+').map(|(_, rest)| rest).unwrap_or(base);
                // When several subsets share a base name, `get_font_set()` yields
                // them in HashMap order, so `or_insert` kept a NONDETERMINISTIC
                // one - the returned bytes changed run to run for the same PDF.
                //
                // Choose by a TOTAL ORDER instead: largest program, ties broken
                // bytewise. Size is only a heuristic for "the richer subset" - a
                // program's byte count also grows with hinting and auxiliary
                // tables, so a larger subset is not necessarily a superset of a
                // smaller one. What the total order does guarantee is the property
                // callers actually depend on: the same PDF always yields the same
                // bytes.
                match by_name.entry(canonical.to_string()) {
                    std::collections::hash_map::Entry::Vacant(v) => {
                        v.insert(data.as_ref().clone());
                    }
                    std::collections::hash_map::Entry::Occupied(mut o) => {
                        let cand = data.as_ref();
                        let cur = o.get();
                        if (cand.len(), cand.as_slice()) > (cur.len(), cur.as_slice()) {
                            *o.get_mut() = cand.clone();
                        }
                    }
                }
            }
        }

        let mut out: Vec<(String, Vec<u8>)> = by_name.into_iter().collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    /// Like [`Self::extract_embedded_fonts`] but additionally returns a
    /// per-font Unicode → GID map reconstructed from the source PDF's
    /// `/ToUnicode` CMap and the font's CID/byte→GID table.
    ///
    /// CFF font subsets in PDFs (the typical Word/LibreOffice output)
    /// often ship without a Unicode cmap because CIDs encode the
    /// glyph stream directly. The font program parses fine but
    /// `EmbeddedFont::glyph_lookup` is empty; downstream font
    /// registration treats the font as unusable and falls back to
    /// Helvetica.
    ///
    /// The map returned here lets office_oxide / pdf_oxide write
    /// pipelines call [`crate::writer::EmbeddedFont::extend_glyph_lookup`]
    /// to re-populate the missing Unicode→GID entries from the
    /// source-PDF's own `/ToUnicode`. Result: CFF subset fonts
    /// register and render with the source typeface program instead
    /// of base-14 Helvetica.
    pub fn extract_embedded_fonts_with_unicode_maps(
        &self,
    ) -> Result<Vec<(String, Vec<u8>, std::collections::HashMap<u32, u16>)>> {
        let with_widths = self.extract_embedded_fonts_with_unicode_maps_and_widths()?;
        Ok(with_widths
            .into_iter()
            .map(|(name, data, uni, _widths)| (name, data, uni))
            .collect())
    }
}
