use super::parsing::*;
use super::preflight::*;
use super::*;

impl PdfDocument {
    /// Extract complete page text data with a specific reading order.
    ///
    /// Like [`extract_page_text`](Self::extract_page_text) but allows choosing
    /// between `TopToBottom` and `ColumnAware` reading order.
    ///
    /// # Arguments
    ///
    /// * `page_index` - Zero-based page index
    /// * `reading_order` - Reading order strategy to apply
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use pdf_oxide::document::{PdfDocument, ReadingOrder};
    /// # fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let mut doc = PdfDocument::open("two_column.pdf")?;
    /// let page_text = doc.extract_page_text_with_options(0, ReadingOrder::ColumnAware)?;
    /// for span in &page_text.spans {
    ///     println!("{}", span.text);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn extract_page_text_with_options(
        &self,
        page_index: usize,
        reading_order: ReadingOrder,
    ) -> Result<crate::layout::PageText> {
        // Get spans with the requested reading order
        let spans = self.extract_spans_with_reading_order(page_index, reading_order)?;

        // Derive chars from spans (uses char_widths for accurate positioning)
        let chars: Vec<crate::layout::TextChar> = spans.iter().flat_map(|s| s.to_chars()).collect();

        // Get page dimensions from MediaBox
        let media_box = self.get_page_media_box(page_index)?;

        Ok(crate::layout::PageText {
            spans,
            chars,
            page_width: media_box.2,
            page_height: media_box.3,
        })
    }

    /// Extract text spans from a page with custom configuration.
    ///
    /// This method allows controlling span merging behavior through configuration,
    /// including adaptive threshold settings for improved extraction quality.
    ///
    /// # Arguments
    ///
    /// * `page_index` - Zero-based page index
    /// * `config` - SpanMergingConfig controlling extraction parameters
    ///
    /// # Returns
    ///
    /// A vector of TextSpan objects extracted from the page with applied configuration.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use pdf_oxide::document::PdfDocument;
    /// # use pdf_oxide::extractors::SpanMergingConfig;
    /// # fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let mut doc = PdfDocument::open("example.pdf")?;
    ///
    /// // Use adaptive threshold configuration
    /// let config = SpanMergingConfig::adaptive();
    /// let spans = doc.extract_spans_with_config(0, config)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn extract_spans_with_config(
        &self,
        page_index: usize,
        config: crate::extractors::SpanMergingConfig,
    ) -> Result<Vec<crate::layout::TextSpan>> {
        use crate::extractors::TextExtractor;

        // Get page object
        let page = self.get_page(page_index)?;
        let page_dict = page.as_dict().ok_or_else(|| Error::ParseError {
            offset: 0,
            reason: "Page is not a dictionary".to_string(),
        })?;

        // Fast pre-check: skip image-only pages before decompression
        if self.page_cannot_have_text(page_dict) {
            return Ok(Vec::new());
        }

        // Get content stream data — skip page on decode failure (Annex I)
        let content_data = match self.get_page_content_data(page_index) {
            Ok(data) => data,
            Err(e) => {
                // Reporting a limit or a cancellation as "no content" tells the caller
                // the page is empty, which is worse than telling them it failed: they
                // stop looking. Degrading stays correct for a malformed stream.
                if matches!(e, Error::ResourceLimit { .. } | Error::Cancelled) {
                    return Err(e);
                }
                log::warn!(
                    "Failed to decode content stream for page {}: {}, returning empty",
                    page_index,
                    e
                );
                return Ok(Vec::new());
            }
        };

        // Early-out for pages with no text content (§9.4.3)
        if !Self::may_contain_text(&content_data) {
            return Ok(Vec::new());
        }

        // Create text extractor with merged configuration
        let mut extractor = TextExtractor::new().with_merging_config(config);

        // Load fonts from page resources and set resources for XObject access
        if let Some(resources) = page_dict.get("Resources") {
            extractor.set_resources(resources.clone());
            extractor.set_document(self);

            // Load fonts
            if let Err(e) = self.load_fonts(resources, &mut extractor) {
                log::warn!(
                    "Failed to load fonts for page {}: {}, continuing with defaults",
                    page_index,
                    e
                );
            }
        }

        // Extract text spans
        extractor.extract_text_spans(&content_data)
    }

    /// Extract individual characters from a PDF page.
    ///
    /// This is a **low-level API** for character-level granularity. For most use cases,
    /// prefer `extract_spans()` which provides complete text strings as PDF defines them.
    ///
    /// # Character-level extraction details:
    ///
    /// - Returns individual `TextChar` objects with position, font, and style information
    /// - Characters are sorted in reading order (top-to-bottom, left-to-right)
    /// - Overlapping characters (rendered multiple times for effects) are deduplicated
    /// - Useful for layout analysis, debugging, or custom text processing pipelines
    ///
    /// # Arguments
    ///
    /// * `page_index` - Page number (0-indexed)
    ///
    /// # Returns
    ///
    /// Vector of `TextChar` objects in reading order, or error if extraction fails
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use pdf_oxide::document::PdfDocument;
    /// # fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let mut doc = PdfDocument::open("document.pdf")?;
    /// let chars = doc.extract_chars(0)?;
    /// for ch in chars {
    ///     println!("'{}' at ({:.1}, {:.1}), font: {}",
    ///         ch.char, ch.bbox.x, ch.bbox.y, ch.font_name);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// List all Optional Content Group (OCG) layer names in the document.
    ///
    /// Reads `/OCProperties` from the document catalog and returns the `/Name`
    /// of each OCG dictionary listed in `/OCGs`. These names can be passed to
    /// `extract_text_filtered` / `extract_chars_filtered` via `excluded_layers`.
    ///
    /// Returns an empty vec if the document has no optional content.
    pub fn get_layers(&self) -> Result<Vec<String>> {
        let catalog = self.catalog()?;
        let catalog_dict = catalog
            .as_dict()
            .ok_or_else(|| Error::InvalidPdf("Catalog is not a dictionary".to_string()))?;

        let oc_props = match catalog_dict.get("OCProperties") {
            Some(obj) => {
                if let Some(r) = obj.as_reference() {
                    self.load_object(r)?
                } else {
                    obj.clone()
                }
            }
            None => return Ok(Vec::new()),
        };

        let oc_dict = match oc_props.as_dict() {
            Some(d) => d,
            None => return Ok(Vec::new()),
        };

        let ocgs_obj = match oc_dict.get("OCGs") {
            Some(obj) => {
                if let Some(r) = obj.as_reference() {
                    self.load_object(r)?
                } else {
                    obj.clone()
                }
            }
            None => return Ok(Vec::new()),
        };

        let ocgs_arr = match ocgs_obj.as_array() {
            Some(a) => a,
            None => return Ok(Vec::new()),
        };

        let mut names = Vec::new();
        for item in ocgs_arr {
            let ocg_obj = if let Some(r) = item.as_reference() {
                match self.load_object(r) {
                    Ok(o) => o,
                    Err(_) => continue,
                }
            } else {
                item.clone()
            };
            if let Some(d) = ocg_obj.as_dict() {
                if let Some(Object::Name(n)) = d.get("Name") {
                    names.push(n.clone());
                } else if let Some(Object::String(s)) = d.get("Name") {
                    if let Ok(text) = String::from_utf8(s.clone()) {
                        names.push(text);
                    }
                }
            }
        }
        Ok(names)
    }

    /// List ink / separation names used on a specific page.
    ///
    /// Scans the page's `/Resources /ColorSpace` dictionary for `/Separation`
    /// and `/DeviceN` color space definitions and returns their ink names.
    /// These names can be passed to `extract_text_filtered` /
    /// `extract_chars_filtered` via `excluded_inks`.
    ///
    /// **Note:** Only the page's own `/Resources` is walked. Spot inks
    /// declared inside a Form XObject's local `/Resources /ColorSpace`
    /// dictionary will not be enumerated — even though the renderer and
    /// extractor will still honor them at use time. Callers populating a
    /// UI picker from this list may miss XObject-local inks.
    ///
    /// For the full walk that follows `Do` operators into Form XObject
    /// resources, use [`Self::get_page_inks_deep`] — that is what the
    /// separation renderer uses to allocate plates.
    pub fn get_page_inks(&self, page_index: usize) -> Result<Vec<String>> {
        let page = self.get_page(page_index)?;
        let page_dict = page.as_dict().ok_or_else(|| Error::ParseError {
            offset: 0,
            reason: "Page is not a dictionary".to_string(),
        })?;

        let resources = match page_dict.get("Resources") {
            Some(r) => {
                if let Some(rr) = r.as_reference() {
                    self.load_object(rr)?
                } else {
                    r.clone()
                }
            }
            None => return Ok(Vec::new()),
        };

        let res_dict = match resources.as_dict() {
            Some(d) => d,
            None => return Ok(Vec::new()),
        };

        let cs_obj = match res_dict.get("ColorSpace") {
            Some(obj) => {
                if let Some(r) = obj.as_reference() {
                    self.load_object(r)?
                } else {
                    obj.clone()
                }
            }
            None => return Ok(Vec::new()),
        };

        let cs_dict = match cs_obj.as_dict() {
            Some(d) => d,
            None => return Ok(Vec::new()),
        };

        // Resolve any indirect references so the extractor sees inline
        // arrays. Mirrors the pre-existing per-entry resolve loop.
        let mut resolved: std::collections::HashMap<String, Object> =
            std::collections::HashMap::with_capacity(cs_dict.len());
        for (name, cs_def) in cs_dict.iter() {
            let v = if let Some(r) = cs_def.as_reference() {
                match self.load_object(r) {
                    Ok(o) => o,
                    Err(_) => continue,
                }
            } else {
                cs_def.clone()
            };
            resolved.insert(name.clone(), v);
        }

        let mut ink_names = Vec::new();
        extract_inks_from_color_space_dict(&resolved, Some(self), &mut ink_names);

        ink_names.sort();
        ink_names.dedup();
        Ok(ink_names)
    }

    /// List ink / separation names declared on a page **including** those
    /// declared inside Form XObjects reached through the page's content-stream
    /// `Do` operators.
    ///
    /// Walks the page's content stream looking for `Do` operators that invoke
    /// Form XObjects (§8.10), recurses into each form's `/Resources/ColorSpace`
    /// dictionary, and accumulates `/Separation` and `/DeviceN` ink names from
    /// every visited resource tree.
    ///
    /// **Cycle handling:** indirect XObject references are deduplicated by
    /// `ObjectRef`; recursion depth is bounded at `MAX_RECURSION_DEPTH` (100).
    /// A cycle below the depth bound is silently terminated; a tree deeper
    /// than the bound returns [`Error::RecursionLimitExceeded`].
    ///
    /// **Out of scope:** tiling / shading patterns (§8.7) and annotation
    /// appearance streams (§12.5.5) — both can declare their own colour
    /// spaces but the separation renderer does not paint into them, so
    /// surfacing their inks here would create plates that stay empty.
    pub fn get_page_inks_deep(&self, page_index: usize) -> Result<Vec<String>> {
        let resources = self.page_resources_for_inks(page_index)?;
        let content_data = self.get_page_content_data(page_index)?;
        let operators = crate::content::parser::parse_content_stream(&content_data)?;

        let mut ink_names: Vec<String> = Vec::new();
        let mut visited: std::collections::HashSet<crate::object::ObjectRef> =
            std::collections::HashSet::new();

        self.collect_inks_from_resources(&resources, &mut ink_names)?;
        self.walk_form_xobject_tree_for_inks(
            &operators,
            &resources,
            &mut ink_names,
            &mut visited,
            0,
        )?;

        ink_names.sort();
        ink_names.dedup();
        Ok(ink_names)
    }

    /// Resolve the page's `/Resources` entry, following an indirect
    /// reference if present. Mirrors the same pattern used by
    /// [`Self::get_page_inks`]. Internal helper that does not depend on
    /// the `rendering`-feature-gated [`Self::get_page_resources`].
    pub(super) fn page_resources_for_inks(&self, page_index: usize) -> Result<Object> {
        let page = self.get_page(page_index)?;
        let page_dict = page.as_dict().ok_or_else(|| Error::ParseError {
            offset: 0,
            reason: "Page is not a dictionary".to_string(),
        })?;
        let resources = match page_dict.get("Resources") {
            Some(r) => match r.as_reference() {
                Some(rr) => self.load_object(rr)?,
                None => r.clone(),
            },
            None => Object::Dictionary(std::collections::HashMap::new()),
        };
        Ok(resources)
    }

    /// Dereference `obj` if it is an indirect reference; otherwise clone.
    /// Internal helper that mirrors the rendering-gated
    /// [`Self::resolve_object`] without taking the gate.
    pub(super) fn deref_object_for_inks(&self, obj: &Object) -> Result<Object> {
        match obj.as_reference() {
            Some(r) => self.load_object(r),
            None => Ok(obj.clone()),
        }
    }

    /// Append inks declared in `resources./ColorSpace` (resolving indirect
    /// references) to `out`. Internal helper for both
    /// [`Self::get_page_inks_deep`] and the recursive form walker.
    pub(super) fn collect_inks_from_resources(
        &self,
        resources: &Object,
        out: &mut Vec<String>,
    ) -> Result<()> {
        let res_dict = match resources.as_dict() {
            Some(d) => d,
            None => return Ok(()),
        };
        let cs_obj = match res_dict.get("ColorSpace") {
            Some(obj) => self.deref_object_for_inks(obj)?,
            None => return Ok(()),
        };
        let cs_dict_raw = match cs_obj.as_dict() {
            Some(d) => d,
            None => return Ok(()),
        };

        let mut resolved: std::collections::HashMap<String, Object> =
            std::collections::HashMap::with_capacity(cs_dict_raw.len());
        for (name, cs_def) in cs_dict_raw.iter() {
            let v = match cs_def.as_reference() {
                Some(r) => match self.load_object(r) {
                    Ok(o) => o,
                    Err(_) => continue,
                },
                None => cs_def.clone(),
            };
            resolved.insert(name.clone(), v);
        }
        extract_inks_from_color_space_dict(&resolved, Some(self), out);
        Ok(())
    }

    /// Recursive walker: for every `Operator::Do { name }` in `operators` that
    /// resolves to a Form XObject, scan that form's `/Resources/ColorSpace`
    /// and recurse into the form's own content stream.
    ///
    /// `visited` is keyed on the XObject's `ObjectRef` (indirect references
    /// only). Inline-stream forms cannot self-reference (no name to invoke);
    /// the depth limit is the backstop for any other malformed shape.
    pub(super) fn walk_form_xobject_tree_for_inks(
        &self,
        operators: &[crate::content::operators::Operator],
        parent_resources: &Object,
        out: &mut Vec<String>,
        visited: &mut std::collections::HashSet<crate::object::ObjectRef>,
        depth: u32,
    ) -> Result<()> {
        if depth >= MAX_RECURSION_DEPTH {
            return Err(Error::RecursionLimitExceeded(MAX_RECURSION_DEPTH));
        }
        let xobjects = match parent_resources.as_dict() {
            Some(rd) => match rd.get("XObject") {
                Some(o) => self.deref_object_for_inks(o)?,
                None => return Ok(()),
            },
            None => return Ok(()),
        };
        let xobj_dict = match xobjects.as_dict() {
            Some(d) => d,
            None => return Ok(()),
        };

        for op in operators {
            let name = match op {
                crate::content::operators::Operator::Do { name } => name,
                _ => continue,
            };
            let xobj_entry = match xobj_dict.get(name) {
                Some(o) => o,
                None => continue,
            };
            let xobj_ref = xobj_entry.as_reference();
            if let Some(r) = xobj_ref {
                // Cycle through indirect refs: silent skip below depth bound.
                if !visited.insert(r) {
                    continue;
                }
            }
            let xobj = match self.deref_object_for_inks(xobj_entry) {
                Ok(o) => o,
                Err(_) => continue,
            };
            let (form_dict, form_stream) = match xobj {
                Object::Stream { ref dict, .. } => {
                    if dict.get("Subtype").and_then(Object::as_name) != Some("Form") {
                        continue;
                    }
                    let data = match xobj_ref {
                        Some(r) => self.decode_stream_with_encryption(&xobj, r)?,
                        None => xobj.decode_stream_data()?,
                    };
                    (dict.clone(), data)
                }
                _ => continue,
            };

            // §8.10.1: form may override resources or inherit the parent's.
            let form_resources = match form_dict.get("Resources") {
                Some(res) => self.deref_object_for_inks(res)?,
                None => parent_resources.clone(),
            };
            self.collect_inks_from_resources(&form_resources, out)?;

            // Recurse into the form's own content stream looking for nested
            // `Do`. Malformed streams are tolerated — we want graceful
            // degradation in a discovery API, not a hard error.
            let form_ops = match crate::content::parser::parse_content_stream(&form_stream) {
                Ok(ops) => ops,
                Err(_) => continue,
            };
            self.walk_form_xobject_tree_for_inks(
                &form_ops,
                &form_resources,
                out,
                visited,
                depth + 1,
            )?;
        }
        Ok(())
    }

    /// # Performance Note
    ///
    /// Character extraction is typically 30-50% faster than span extraction
    /// because it skips the text grouping and merging logic.
    pub fn extract_chars(&self, page_index: usize) -> Result<Vec<crate::layout::TextChar>> {
        Ok((*self.cached_page_chars(page_index)?).clone())
    }

    /// Shared, cached character sequence for a page — identical to what
    /// [`Self::extract_chars`] returns, minus the clone. Only the unfiltered
    /// extraction is cached; the layer/ink-filtered variant is keyed on its
    /// filters and stays uncached.
    pub(super) fn cached_page_chars(
        &self,
        page_index: usize,
    ) -> Result<std::sync::Arc<Vec<crate::layout::TextChar>>> {
        if let Some(cached) = self.page_chars_cache.lock_or_recover().get(&page_index) {
            return Ok(std::sync::Arc::clone(cached));
        }
        let chars = std::sync::Arc::new(self.extract_chars_impl(
            page_index,
            HashSet::new(),
            HashSet::new(),
        )?);
        self.page_chars_cache
            .lock_or_recover()
            .insert(page_index, std::sync::Arc::clone(&chars));
        Ok(chars)
    }

    /// Extract characters from a page, excluding content from specified layers and inks.
    ///
    /// # Arguments
    ///
    /// * `page_index` - Zero-based page index
    /// * `excluded_layers` - OCG layer names to suppress (empty = no layer filtering)
    /// * `excluded_inks` - Separation/DeviceN ink names to suppress (empty = no ink filtering)
    pub fn extract_chars_filtered(
        &self,
        page_index: usize,
        excluded_layers: HashSet<String>,
        excluded_inks: HashSet<String>,
    ) -> Result<Vec<crate::layout::TextChar>> {
        self.extract_chars_impl(page_index, excluded_layers, excluded_inks)
    }

    pub(super) fn extract_chars_impl(
        &self,
        page_index: usize,
        excluded_layers: HashSet<String>,
        excluded_inks: HashSet<String>,
    ) -> Result<Vec<crate::layout::TextChar>> {
        use crate::extractors::TextExtractor;

        let page = self.get_page(page_index)?;
        let page_dict = page.as_dict().ok_or_else(|| Error::ParseError {
            offset: 0,
            reason: "Page is not a dictionary".to_string(),
        })?;

        let content_data = match self.get_page_content_data(page_index) {
            Ok(data) => data,
            Err(e) => {
                // Reporting a limit or a cancellation as "no content" tells the caller
                // the page is empty, which is worse than telling them it failed: they
                // stop looking. Degrading stays correct for a malformed stream.
                if matches!(e, Error::ResourceLimit { .. } | Error::Cancelled) {
                    return Err(e);
                }
                log::warn!(
                    "Failed to decode content stream for page {}: {}, returning empty",
                    page_index,
                    e
                );
                return Ok(Vec::new());
            }
        };

        if !Self::may_contain_text(&content_data) {
            return Ok(Vec::new());
        }

        let mut extractor = TextExtractor::new();
        if !excluded_layers.is_empty() {
            extractor.set_excluded_layers(excluded_layers);
        }
        if !excluded_inks.is_empty() {
            extractor.set_excluded_inks(excluded_inks);
        }

        if let Some(resources) = page_dict.get("Resources") {
            extractor.set_resources(resources.clone());
            extractor.set_document(self);
            if let Err(e) = self.load_fonts(resources, &mut extractor) {
                log::warn!(
                    "Failed to load fonts for page {}: {}, continuing with defaults",
                    page_index,
                    e
                );
            }
        }

        let mut chars = extractor.extract_owned(&content_data)?;

        chars.sort_by(|a, b| {
            let y_cmp = crate::utils::safe_float_cmp(b.bbox.y, a.bbox.y);
            if y_cmp != std::cmp::Ordering::Equal {
                return y_cmp;
            }
            crate::utils::safe_float_cmp(a.bbox.x, b.bbox.x)
        });

        Ok(chars)
    }

    /// Extract words from a page.
    ///
    /// Groups characters into words based on spatial proximity.
    /// Uses adaptive thresholds based on the document's font size and spacing.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let words = doc.extract_words(0)?;
    /// for word in words {
    ///     println!("Word: {} at {:?}", word.text, word.bbox);
    /// }
    /// ```
    pub fn extract_words(&self, page_index: usize) -> Result<Vec<crate::layout::Word>> {
        self.extract_words_with_thresholds(page_index, None, None)
    }

    /// Extract words from a page with optional threshold and profile overrides.
    ///
    /// When `word_gap_threshold` is `None`, the adaptive threshold is computed
    /// automatically from page statistics (median character width × 0.3).
    /// Providing a value (in PDF points) overrides the adaptive computation,
    /// which is useful for tuning word segmentation on specific document types.
    ///
    /// When `profile` is provided, it controls how the underlying text spans are
    /// extracted from the PDF content stream (TJ offset thresholds, word margin
    /// ratios). This affects the raw character data before word clustering.
    pub fn extract_words_with_thresholds(
        &self,
        page_index: usize,
        word_gap_threshold: Option<f32>,
        profile: Option<crate::config::ExtractionProfile>,
    ) -> Result<Vec<crate::layout::Word>> {
        // Default: include /Artifact-tagged spans (matches pre-0.3.42
        // behavior). The spec-correct (§14.8.2.2.1) variant lives in
        // [`Self::extract_words_with_thresholds_no_artifacts`].
        self.extract_words_inner(page_index, word_gap_threshold, profile, true)
    }

    /// Same as [`Self::extract_words_with_thresholds`] but drops spans tagged
    /// as `/Artifact` (running headers/footers, page numbers, watermarks;
    /// ISO 32000-1:2008 §14.8.2.2.1). The spec-correct variant.
    pub fn extract_words_with_thresholds_no_artifacts(
        &self,
        page_index: usize,
        word_gap_threshold: Option<f32>,
        profile: Option<crate::config::ExtractionProfile>,
    ) -> Result<Vec<crate::layout::Word>> {
        self.extract_words_inner(page_index, word_gap_threshold, profile, false)
    }
}
