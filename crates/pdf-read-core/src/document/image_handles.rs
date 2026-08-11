use super::parsing::*;
use super::preflight::*;
use super::*;

impl PdfDocument {
    /// Enumerate images on a page without decompressing any stream (Phase 1).
    ///
    /// Walks the page content stream once and reads image metadata (dimensions,
    /// colour space, filter chain, compressed size) directly from each Image
    /// XObject dictionary. No pixel data is decoded. Returns a handle per image
    /// in content-stream paint order.
    ///
    /// Call [`crate::PdfImageHandle::decode`] on individual handles to materialise only
    /// the images you need, or [`crate::PdfImageHandle::raw_compressed_bytes`] to forward
    /// compressed data (e.g. JPEG bytes) without recompression.
    ///
    /// Form XObjects (subtype `/Form`) are recursed into, matching the behaviour
    /// of [`PdfDocument::extract_images`]. Cycle detection (depth limit 100) and
    /// the document's Form stream cache are used. Images inside nested or shared
    /// Forms receive the correct final CTM-composed `bbox` / `rotation_degrees`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use pdf_oxide::PdfDocument;
    /// # let bytes = std::fs::read("page.pdf").unwrap();
    /// let doc = PdfDocument::from_bytes(bytes).unwrap();
    ///
    /// // Decode only images larger than a thumbnail threshold
    /// let images: Vec<_> = doc.page_image_handles(0)?
    ///     .into_iter()
    ///     .filter(|h| h.width >= 200 && h.height >= 200)
    ///     .map(|h| h.decode())
    ///     .collect::<Result<_, _>>()?;
    /// # Ok::<(), pdf_oxide::error::Error>(())
    /// ```
    pub fn page_image_handles(
        &self,
        page_index: usize,
    ) -> Result<Vec<crate::extractors::images::PdfImageHandle<'_>>> {
        use crate::content::parse_content_stream_images_only;
        use crate::content::Operator;
        use crate::extractors::images::image_handle_from_inline;

        self.require_authenticated()?;

        let page = self.get_page(page_index)?;
        let page_dict = page.as_dict().ok_or_else(|| Error::ParseError {
            offset: 0,
            reason: "Page is not a dictionary".to_string(),
        })?;

        let content_data = self.get_page_content_data(page_index)?;

        let resources = match page_dict.get("Resources") {
            Some(res) => {
                if let Some(ref_obj) = res.as_reference() {
                    Some(self.load_object(ref_obj)?)
                } else {
                    Some(res.clone())
                }
            }
            None => None,
        };

        let operators = match parse_content_stream_images_only(&content_data) {
            Ok(ops) => ops,
            Err(_) => return Ok(Vec::new()),
        };

        // Resource-name colour-space map for this page scope (§8.6.6 / §8.9.7).
        let cs_map = self.build_color_space_map(resources.as_ref());

        // Pre-resolve the XObject dictionary once
        let xobject_dict = if let Some(ref res) = resources {
            if let Some(res_dict) = res.as_dict() {
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
            }
        } else {
            None
        };

        let mut handles = Vec::new();
        let mut ctm_stack = vec![crate::content::Matrix::identity()];
        let mut paint_order: usize = 0;
        let mut xobject_stack: Vec<crate::object::ObjectRef> = Vec::new();

        for op in operators {
            match op {
                Operator::SaveState => {
                    if let Some(current) = ctm_stack.last() {
                        ctm_stack.push(*current);
                    }
                }
                Operator::RestoreState => {
                    if ctm_stack.len() > 1 {
                        ctm_stack.pop();
                    }
                }
                Operator::Cm { a, b, c, d, e, f } => {
                    if let Some(current) = ctm_stack.last_mut() {
                        let m = crate::content::Matrix { a, b, c, d, e, f };
                        *current = m.multiply(current);
                    }
                }
                Operator::Do { name } => {
                    if let Some(ref xobj_dict_map) = xobject_dict {
                        let ctm = ctm_stack
                            .last()
                            .copied()
                            .unwrap_or_else(crate::content::Matrix::identity);
                        if let Ok(mut more) = self.collect_handles_from_do(
                            &name,
                            xobj_dict_map,
                            resources.as_ref(),
                            ctm,
                            &mut paint_order,
                            &mut xobject_stack,
                        ) {
                            handles.append(&mut more);
                        }
                    }
                }
                Operator::InlineImage { dict, data } => {
                    let ctm = ctm_stack
                        .last()
                        .copied()
                        .unwrap_or_else(crate::content::Matrix::identity);
                    if let Some(handle) =
                        image_handle_from_inline(self, &dict, data, ctm, paint_order, &cs_map)
                    {
                        handles.push(handle);
                        paint_order += 1;
                    }
                }
                _ => {}
            }
        }

        Ok(handles)
    }

    /// Collect zero or more image handles for a `Do` operator.
    ///
    /// If the target is an Image XObject, returns a vec containing one handle
    /// (paint_order is advanced). If it is a Form XObject, recurses and returns
    /// all image handles found inside (including nested Forms), with correct
    /// paint_order and CTM composition for every handle.
    pub(super) fn collect_handles_from_do<'s>(
        &'s self,
        name: &str,
        xobject_dict: &std::collections::HashMap<String, Object>,
        resources: Option<&Object>,
        ctm: crate::content::Matrix,
        paint_order: &mut usize,
        xobject_stack: &mut Vec<crate::object::ObjectRef>,
    ) -> Result<Vec<crate::extractors::images::PdfImageHandle<'s>>> {
        use crate::extractors::images::image_handle_from_xobject;

        let xobject_ref_obj = match xobject_dict.get(name) {
            Some(o) => o,
            None => return Ok(Vec::new()),
        };

        let xobject_ref_opt = xobject_ref_obj.as_reference();
        let xobject = if let Some(ref_obj) = xobject_ref_opt {
            self.load_object(ref_obj)?
        } else {
            xobject_ref_obj.clone()
        };
        let xobj_dict = match xobject.as_dict() {
            Some(d) => d,
            None => return Ok(Vec::new()),
        };

        let subtype = xobj_dict
            .get("Subtype")
            .and_then(|s| s.as_name())
            .unwrap_or("");

        match subtype {
            "Image" => {
                if let Some(ref_obj) = xobject_ref_opt {
                    let cs_map = self.build_color_space_map(resources);
                    if let Some(h) = image_handle_from_xobject(
                        self,
                        ref_obj,
                        xobj_dict,
                        ctm,
                        *paint_order,
                        &cs_map,
                    ) {
                        *paint_order += 1;
                        Ok(vec![h])
                    } else {
                        Ok(Vec::new())
                    }
                } else {
                    Ok(Vec::new())
                }
            }
            "Form" => {
                if let (Some(ref_obj), Some(parent_res)) = (xobject_ref_opt, resources) {
                    self.collect_image_handles_from_form_xobject(
                        ref_obj,
                        &xobject,
                        parent_res,
                        ctm,
                        paint_order,
                        xobject_stack,
                    )
                } else {
                    Ok(Vec::new())
                }
            }
            _ => Ok(Vec::new()),
        }
    }

    /// Recursively collect image handles from a Form XObject.
    ///
    /// This is the handles-side equivalent of `extract_images_from_form_xobject`.
    /// It uses the same cycle detection (ObjectRef stack + depth 100), the same
    /// Form Resources fallback rules, the same Form /Matrix handling, and reuses
    /// the document's xobject_stream_cache (50 MiB bound) for decompressed Form
    /// content.
    ///
    /// Unlike the materialised path, we do not cache "raw" handles — we compose
    /// the full CTM (`parent_ctm * form_matrix`) at entry and let every inner
    /// handle (and nested Form) naturally receive the final geometry. This is
    /// simpler for the two-phase API and produces correct `bbox`/`rotation_degrees`
    /// / `ctm` fields on the returned handles.
    pub(super) fn collect_image_handles_from_form_xobject<'s>(
        &'s self,
        xobject_ref: crate::object::ObjectRef,
        xobject: &Object,
        parent_resources: &Object,
        parent_ctm: crate::content::Matrix,
        paint_order: &mut usize,
        xobject_stack: &mut Vec<crate::object::ObjectRef>,
    ) -> Result<Vec<crate::extractors::images::PdfImageHandle<'s>>> {
        use crate::content::parse_content_stream_images_only;
        use crate::content::Operator;
        use crate::extractors::images::image_handle_from_inline;

        // Cycle detection — identical policy to the materialised extraction path.
        if xobject_stack.contains(&xobject_ref) || xobject_stack.len() >= 100 {
            return Ok(Vec::new());
        }

        xobject_stack.push(xobject_ref);

        let xobj_dict = match xobject.as_dict() {
            Some(d) => d,
            None => {
                xobject_stack.pop();
                return Ok(Vec::new());
            }
        };

        // Form's own Resources (fallback to the parent's resources if absent).
        let form_resources = if let Some(form_res) = xobj_dict.get("Resources") {
            if let Some(ref_obj) = form_res.as_reference() {
                self.load_object(ref_obj)?
            } else {
                form_res.clone()
            }
        } else {
            parent_resources.clone()
        };

        // Pre-resolve the XObject dictionary for *this* Form's Resources.
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

        // Form's own transformation matrix (default identity).
        let form_matrix = if let Some(matrix_obj) = xobj_dict.get("Matrix") {
            self.parse_matrix_from_object(matrix_obj)
                .unwrap_or_else(crate::content::Matrix::identity)
        } else {
            crate::content::Matrix::identity()
        };

        // Decode the Form stream (respecting the 50 MiB document-level cache).
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

        // Parse with the fast images-only parser (same as the materialised path).
        let operators = match parse_content_stream_images_only(&stream_data) {
            Ok(ops) => ops,
            Err(_) => {
                xobject_stack.pop();
                return Ok(Vec::new());
            }
        };

        // Critical CTM composition:
        // Start the form's internal graphics state with `parent_ctm * form_matrix`.
        // Every image (and nested Form) discovered inside will then have its
        // handle's bbox/rotation/ctm computed with the *final* transform that
        // will be active when the image is painted on the page.
        let start_ctm = parent_ctm.multiply(&form_matrix);
        let mut ctm_stack = vec![start_ctm];
        let mut handles = Vec::new();

        for op in operators {
            match op {
                Operator::SaveState => {
                    if let Some(current) = ctm_stack.last() {
                        ctm_stack.push(*current);
                    }
                }
                Operator::RestoreState => {
                    if ctm_stack.len() > 1 {
                        ctm_stack.pop();
                    }
                }
                Operator::Cm { a, b, c, d, e, f } => {
                    if let Some(current) = ctm_stack.last_mut() {
                        let m = crate::content::Matrix { a, b, c, d, e, f };
                        *current = m.multiply(current);
                    }
                }

                Operator::Do { name } => {
                    if let Some(ref xobj_d) = form_xobject_dict {
                        let current_ctm = ctm_stack
                            .last()
                            .copied()
                            .unwrap_or_else(crate::content::Matrix::identity);
                        if let Ok(mut more) = self.collect_handles_from_do(
                            &name,
                            xobj_d,
                            Some(&form_resources),
                            current_ctm,
                            paint_order,
                            xobject_stack,
                        ) {
                            handles.append(&mut more);
                        }
                    }
                }

                Operator::InlineImage { dict, data } => {
                    let current_ctm = ctm_stack
                        .last()
                        .copied()
                        .unwrap_or_else(crate::content::Matrix::identity);
                    let cs_map = self.build_color_space_map(Some(&form_resources));
                    if let Some(h) = image_handle_from_inline(
                        self,
                        &dict,
                        data,
                        current_ctm,
                        *paint_order,
                        &cs_map,
                    ) {
                        handles.push(h);
                        *paint_order += 1;
                    }
                }

                _ => {}
            }
        }

        xobject_stack.pop();
        Ok(handles)
    }

    /// Extract images with pre-decompression filtering.
    ///
    /// Applies dimension and pixel-count checks using XObject dictionary metadata
    /// BEFORE expensive stream decompression. This avoids decompressing oversized
    /// images (e.g., 36MP presentation slides) or tiny glyph fragments that will
    /// be discarded downstream.
    pub(super) fn extract_images_filtered(
        &self,
        page_index: usize,
        filter: &ImageExtractFilter,
    ) -> Result<Vec<crate::extractors::PdfImage>> {
        use crate::content::parse_content_stream_images_only;
        use crate::content::Operator;

        // Get page object and resources
        let page = self.get_page(page_index)?;
        let page_dict = page.as_dict().ok_or_else(|| Error::ParseError {
            offset: 0,
            reason: "Page is not a dictionary".to_string(),
        })?;

        // Get content stream
        let content_data = self.get_page_content_data(page_index)?;

        // Resolve resources
        let resources = match page_dict.get("Resources") {
            Some(res) => {
                if let Some(ref_obj) = res.as_reference() {
                    Some(self.load_object(ref_obj)?)
                } else {
                    Some(res.clone())
                }
            }
            None => None,
        };

        // Parse content stream with image-only fast path (skips BT/ET text blocks)
        let operators = match parse_content_stream_images_only(&content_data) {
            Ok(ops) => ops,
            Err(_) => {
                // If content stream parsing fails, return empty
                return Ok(Vec::new());
            }
        };

        let mut images = Vec::new();
        let mut ctm_stack = vec![crate::content::Matrix::identity()];
        // Shared cycle detection stack for Form XObject recursion.
        // This must persist across all Do operator calls to detect circular references
        // (e.g., Form X0 references X1 which references X0).
        let mut xobject_stack = Vec::new();

        // Pre-resolve XObject dictionary once (avoids re-resolving per Do operator)
        let xobject_dict = if let Some(ref res) = resources {
            if let Some(res_dict) = res.as_dict() {
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
            }
        } else {
            None
        };

        // Parse content stream operators to extract images from Do operators
        for op in operators {
            match op {
                // Graphics state operators
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

                // XObject reference operator - Extract images referenced via Do
                Operator::Do { name } => {
                    if let Some(ref xobj_dict) = xobject_dict {
                        let current_ctm = ctm_stack
                            .last()
                            .copied()
                            .unwrap_or_else(crate::content::Matrix::identity);
                        if let Ok(mut xobj_images) = self.extract_images_from_xobject_do(
                            &name,
                            xobj_dict,
                            resources.as_ref(),
                            current_ctm,
                            &mut xobject_stack,
                            filter,
                        ) {
                            images.append(&mut xobj_images);
                        }
                    }
                }

                // Inline image operator
                Operator::InlineImage { dict, data } => {
                    let current_ctm = ctm_stack
                        .last()
                        .copied()
                        .unwrap_or_else(crate::content::Matrix::identity);
                    if let Ok(image) = self.extract_image_from_inline(&dict, &data, current_ctm) {
                        images.push(image);
                    }
                }

                _ => {} // Ignore other operators
            }
        }

        Ok(images)
    }

    /// Extract images referenced by a Do operator in the content stream.
    ///
    /// Accepts a pre-resolved XObject dictionary to avoid redundant lookups
    /// when called repeatedly (e.g., 194 Do operators on a single page).
    pub(super) fn extract_images_from_xobject_do(
        &self,
        name: &str,
        xobject_dict: &std::collections::HashMap<String, Object>,
        resources: Option<&Object>,
        ctm: crate::content::Matrix,
        xobject_stack: &mut Vec<ObjectRef>,
        filter: &ImageExtractFilter,
    ) -> Result<Vec<crate::extractors::PdfImage>> {
        use crate::extractors::extract_image_from_xobject;

        let mut images = Vec::new();

        // Get the specific XObject by name
        let xobject_ref_obj = match xobject_dict.get(name) {
            Some(obj) => obj,
            None => return Ok(images), // Named XObject not found
        };

        // Load XObject (can be indirect reference or direct object)
        let xobject_ref_opt = xobject_ref_obj.as_reference();
        let xobject = if let Some(ref_obj) = xobject_ref_opt {
            self.load_object(ref_obj)?
        } else {
            xobject_ref_obj.clone()
        };
        let xobject_dict = xobject.as_dict().ok_or_else(|| Error::ParseError {
            offset: 0,
            reason: "XObject is not a dictionary".to_string(),
        })?;

        // Check Subtype
        let subtype = xobject_dict
            .get("Subtype")
            .and_then(|s| s.as_name())
            .unwrap_or("");

        match subtype {
            "Image" => {
                // Pre-decompression filtering using dictionary metadata.
                // These checks use Width/Height/ColorSpace from the XObject dictionary
                // which are available WITHOUT decompressing the image stream data.
                let w = xobject_dict
                    .get("Width")
                    .and_then(|o| o.as_integer())
                    .unwrap_or(0);
                let h = xobject_dict
                    .get("Height")
                    .and_then(|o| o.as_integer())
                    .unwrap_or(0);
                if w < filter.min_width || h < filter.min_height {
                    return Ok(images);
                }
                if (w as u64) * (h as u64) > filter.max_pixels {
                    return Ok(images);
                }
                // Skip small Indexed colorspace images (Type3 font glyph fragments)
                if filter.skip_indexed_small > 0
                    && (w < filter.skip_indexed_small || h < filter.skip_indexed_small)
                {
                    if let Some(cs_obj) = xobject_dict.get("ColorSpace") {
                        let is_indexed = match cs_obj {
                            Object::Name(n) => n == "Indexed",
                            Object::Array(arr) if !arr.is_empty() => {
                                arr[0].as_name() == Some("Indexed")
                            }
                            _ => false,
                        };
                        if is_indexed {
                            return Ok(images);
                        }
                    }
                }

                // Only clone+modify when ColorSpace needs resolving from indirect ref
                let needs_cs_resolve = matches!(
                    &xobject,
                    Object::Stream { dict, .. } if matches!(dict.get("ColorSpace"), Some(Object::Reference(_)))
                );

                let resolved_xobject;
                let xobject_for_extract = if needs_cs_resolve {
                    if let Object::Stream { dict, data } = &xobject {
                        let mut new_dict = dict.clone();
                        if let Some(Object::Reference(cs_ref)) = dict.get("ColorSpace") {
                            if let Ok(resolved_cs) = self.load_object(*cs_ref) {
                                new_dict.insert("ColorSpace".to_string(), resolved_cs);
                            }
                        }
                        resolved_xobject = Object::Stream {
                            dict: new_dict,
                            data: data.clone(),
                        };
                        &resolved_xobject
                    } else {
                        &xobject
                    }
                } else {
                    &xobject
                };

                // Extract as Image XObject
                if let Ok(mut image) = extract_image_from_xobject(
                    Some(self),
                    xobject_for_extract,
                    xobject_ref_opt,
                    None,
                ) {
                    // In PDF, images are mapped from unit square (0,0 to 1,1) to the CTM.
                    let unit_rect = crate::geometry::Rect::new(0.0, 0.0, 1.0, 1.0);
                    let bbox = self.transform_bbox_with_ctm(&unit_rect, ctm);
                    image.set_bbox(bbox);

                    // Capture transformation matrix and rotation (v0.3.14)
                    image.set_matrix([ctm.a, ctm.b, ctm.c, ctm.d, ctm.e, ctm.f]);
                    image.set_rotation_degrees(Self::matrix_to_rotation(ctm));

                    images.push(image);
                }
            }
            "Form" => {
                // Recursively extract from Form XObject
                // Only process if we have a valid reference and parent resources
                if let (Some(ref_obj), Some(parent_res)) = (xobject_ref_opt, resources) {
                    if let Ok(mut form_images) = self.extract_images_from_form_xobject(
                        ref_obj,
                        &xobject,
                        parent_res,
                        ctm,
                        xobject_stack,
                        filter,
                    ) {
                        images.append(&mut form_images);
                    }
                }
            }
            _ => {} // Skip other types (PS, etc.)
        }

        Ok(images)
    }
}
