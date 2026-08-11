use super::parsing::*;
use super::preflight::*;
use super::*;

impl PdfDocument {
    /// Returns the document's structure tree **only when it is trustworthy for
    /// reading-order purposes**, per ISO 32000-1:2008 §14.8.2.3.1 and §14.7.1.
    ///
    /// A `/StructTreeRoot` encodes the producer's *logical structure order* — a
    /// depth-first traversal of the tag hierarchy — which is authoritative for
    /// reading order independent of glyph geometry (§14.7.1). It is trusted when
    /// the document is `/Marked` (Tagged PDF) **or** the catalog directly
    /// references a `/StructTreeRoot` (PDF 1.3/1.4 tagged files predate the
    /// `/MarkInfo` dictionary; §7.7.2) — matching the historical gate so output
    /// for non-suspect documents is byte-for-byte unchanged — **and**
    /// `/MarkInfo /Suspects` is not `true`. A `true` `/Suspects` flag is the
    /// spec-sanctioned signal (the `/TagSuspect /Ordering` mechanism,
    /// §14.8.2.3.1) that page content order may not match logical structure
    /// order, so the tree is rejected and callers fall back to geometric order.
    ///
    /// Shares `structure_tree_cache`, so this costs a single cached parse.
    pub(crate) fn struct_tree_trustworthy(&self) -> Option<Arc<crate::structure::StructTreeRoot>> {
        let mark = self.mark_info().unwrap_or_default();
        // Suspect documents: geometric reading order is spec-correct
        // (§14.8.2.3.1). This is the only behavioural change versus the legacy
        // inline gate, which never consulted /Suspects.
        if mark.suspects {
            return None;
        }
        let cached = self.structure_tree_cache.lock_or_recover().clone();
        match cached {
            Some(tree) => tree,
            None => {
                let has_struct_tree_root = self
                    .catalog()
                    .ok()
                    .and_then(|cat| cat.as_dict().map(|d| d.contains_key("StructTreeRoot")))
                    .unwrap_or(false);
                let tree = if mark.marked || has_struct_tree_root {
                    self.structure_tree().ok().flatten().map(Arc::new)
                } else {
                    None
                };
                *self.structure_tree_cache.lock_or_recover() = Some(tree.clone());
                tree
            }
        }
    }

    /// Returns the document's structure tree whenever it is **available**,
    /// independent of `/MarkInfo /Suspects`.
    ///
    /// The `/Suspects` flag (§14.7.1) signals that the producer's *reading
    /// order* may be unreliable, so `struct_tree_trustworthy` rejects the
    /// tree for ordering. `/ActualText`, however, is content replacement
    /// (§14.9.4) and remains trustworthy: a producer that bothered to
    /// supply the replacement text for a glyph run is asserting what
    /// that run is *meant* to read as, regardless of whether sibling
    /// reading-order tags are reliable. This accessor lets the
    /// ActualText pipeline honour the producer's intent on Suspects=true
    /// documents while geometric reading order takes over the ordering
    /// problem.
    ///
    /// Shares `structure_tree_cache` with `struct_tree_trustworthy`, so
    /// both predicates cost a single cached parse.
    pub(crate) fn struct_tree_marked(&self) -> Option<Arc<crate::structure::StructTreeRoot>> {
        let cached = self.structure_tree_cache.lock_or_recover().clone();
        match cached {
            Some(tree) => tree,
            None => {
                let mark = self.mark_info().unwrap_or_default();
                let has_struct_tree_root = self
                    .catalog()
                    .ok()
                    .and_then(|cat| cat.as_dict().map(|d| d.contains_key("StructTreeRoot")))
                    .unwrap_or(false);
                let tree = if mark.marked || has_struct_tree_root {
                    self.structure_tree().ok().flatten().map(Arc::new)
                } else {
                    None
                };
                *self.structure_tree_cache.lock_or_recover() = Some(tree.clone());
                tree
            }
        }
    }

    /// Returns the cached [`ActualTextIndex`] for this document.
    ///
    /// Builds the index lazily on first call, then serves cached copies.
    /// Returns `None` for untagged documents and for tagged documents
    /// whose structure tree carries no `/ActualText`.
    ///
    /// Decoupled from `/MarkInfo /Suspects` — see [`struct_tree_marked`].
    pub(crate) fn actualtext_index(&self) -> Option<Arc<crate::structure::ActualTextIndex>> {
        if let Some(cached) = self.actualtext_index_cache.lock_or_recover().clone() {
            return cached;
        }
        let tree = self.struct_tree_marked();
        let built = tree.and_then(|t| {
            let idx = crate::structure::traversal::build_actualtext_index(&t);
            if idx.is_empty() {
                None
            } else {
                Some(Arc::new(idx))
            }
        });
        *self.actualtext_index_cache.lock_or_recover() = Some(built.clone());
        built
    }

    /// Whether text extraction uses the Tagged-PDF *logical structure order* (a
    /// depth-first traversal of `/StructTreeRoot`) rather than geometric
    /// page-content order for this document.
    ///
    /// Returns `true` exactly when the document carries a trustworthy structure
    /// tree per ISO 32000-1:2008 §14.8.2.3.1 / §14.7.1: it is `/Marked` or the
    /// catalog references a `/StructTreeRoot`, the tree resolves non-empty, and
    /// `/MarkInfo /Suspects` is not `true`. When `false`, extraction falls back
    /// to geometric reading order. This is a read-only introspection accessor;
    /// it does not change extraction behaviour.
    pub fn prefers_structure_reading_order(&self) -> bool {
        self.struct_tree_trustworthy().is_some()
    }

    /// Find the document's default CMYK output-intent profile.
    ///
    /// Per ISO 32000-1:2008 §14.11.5, an `/OutputIntents` array in the
    /// catalog advertises the colour characteristics of the target
    /// output device. Each entry is a dictionary; the `DestOutputProfile`
    /// key (when present) references an ICC profile stream identifying
    /// the intended press / display calibration.
    ///
    /// This method returns the **first CMYK** `DestOutputProfile` it
    /// finds (N = 4) — the usual match for "here is how my CMYK ink
    /// should look" on PDF/X files. Callers can use it as a fallback
    /// profile for plain `/DeviceCMYK` images that lack their own ICC
    /// colour space.
    ///
    /// Returns `None` when no output intent exists, no CMYK entry is
    /// present, or the profile stream can't be parsed as ICC.
    pub fn output_intent_cmyk_profile(&self) -> Option<std::sync::Arc<crate::color::IccProfile>> {
        // Memoise the (potentially expensive) decode + parse: hot rendering
        // paths consult this accessor once per paint, and qcms / lcms2
        // header validation + LUT decode on a hundreds-of-KB profile is
        // not free. `Some(None)` means "checked once, no usable CMYK
        // OutputIntent"; a subsequent call must NOT re-walk the catalog.
        if let Some(cached) = self
            .output_intent_cmyk_profile_cache
            .lock_or_recover()
            .as_ref()
        {
            return cached.clone();
        }
        let resolved = self.compute_output_intent_cmyk_profile();
        *self.output_intent_cmyk_profile_cache.lock_or_recover() = Some(resolved.clone());
        resolved
    }

    /// True when the document catalog declares an `/OutputIntents`
    /// array, regardless of whether the contained profile bytes
    /// successfully parse. Coupled with
    /// [`Self::output_intent_cmyk_profile`] returning `None`, this
    /// distinguishes "no OutputIntent requested" (acceptable silent
    /// fallback) from "OutputIntent requested but unusable" (degraded
    /// press output that callers should warn about). Tracks upstream
    /// issue yfedoseev/pdf_oxide#712 on swallowed profile-parse
    /// diagnostics.
    pub fn has_output_intents_declaration(&self) -> bool {
        let Ok(catalog) = self.catalog() else {
            return false;
        };
        let Some(cat_dict) = catalog.as_dict() else {
            return false;
        };
        let Some(intents_obj) = cat_dict.get("OutputIntents") else {
            return false;
        };
        let intents_obj = match intents_obj {
            Object::Reference(r) => match self.load_object(*r) {
                Ok(o) => o,
                Err(_) => return false,
            },
            other => other.clone(),
        };
        matches!(intents_obj, Object::Array(_))
    }

    pub(super) fn compute_output_intent_cmyk_profile(
        &self,
    ) -> Option<std::sync::Arc<crate::color::IccProfile>> {
        let catalog = self.catalog().ok()?;
        let cat_dict = catalog.as_dict()?;

        let intents_obj = cat_dict.get("OutputIntents")?;
        let intents_obj = match intents_obj {
            Object::Reference(r) => self.load_object(*r).ok()?,
            _ => intents_obj.clone(),
        };
        let intents_arr = match &intents_obj {
            Object::Array(a) => a.clone(),
            _ => return None,
        };

        for entry in intents_arr {
            let entry = match entry {
                // Skip a broken entry rather than aborting the whole array (§7.3.10).
                Object::Reference(r) => match self.load_object(r) {
                    Ok(o) => o,
                    Err(e) => {
                        log::warn!("OutputIntents entry {r} could not be loaded ({e:?}); skipping");
                        continue;
                    }
                },
                other => other,
            };
            let entry_dict = match entry.as_dict() {
                Some(d) => d.clone(),
                None => continue,
            };
            let profile_obj = match entry_dict.get("DestOutputProfile") {
                Some(p) => p.clone(),
                None => continue,
            };
            let profile_label = match &profile_obj {
                Object::Reference(r) => format!("DestOutputProfile {r}"),
                _ => "inline DestOutputProfile".to_string(),
            };
            let profile_stream = match profile_obj {
                Object::Reference(r) => {
                    match self.load_object(r) {
                        Ok(o) => o,
                        Err(e) => {
                            log::warn!("OutputIntent {profile_label} could not be loaded ({e:?}); skipping");
                            continue;
                        }
                    }
                }
                other => other,
            };

            let Object::Stream { dict, .. } = &profile_stream else {
                continue;
            };
            let n = match dict.get("N").and_then(|o| o.as_integer()) {
                Some(4) => 4u8, // only CMYK; ignore RGB/Gray output intents here
                _ => continue,
            };
            let bytes = match profile_stream.decode_stream_data() {
                Ok(b) => b,
                Err(e) => {
                    log::warn!(
                        "OutputIntent {profile_label} stream failed to decode ({e:?}); skipping"
                    );
                    continue;
                }
            };
            match crate::color::IccProfile::parse(bytes, n) {
                Some(prof) => return Some(std::sync::Arc::new(prof)),
                None => {
                    log::warn!(
                        "OutputIntent {profile_label} is not a valid N=4 ICC profile; skipping"
                    )
                }
            }
        }
        None
    }

    /// Get the MarkInfo dictionary from the document catalog.
    ///
    /// The MarkInfo dictionary indicates whether the document conforms to
    /// Tagged PDF conventions and whether the structure tree might contain
    /// suspect (unreliable) content.
    ///
    /// Per ISO 32000-1:2008 Section 14.7.1, the MarkInfo dictionary contains:
    /// - `/Marked` - Whether the document conforms to Tagged PDF conventions
    /// - `/Suspects` - Whether the document contains suspect content
    /// - `/UserProperties` - Whether the document uses user properties
    ///
    /// # Returns
    ///
    /// Returns `MarkInfo` with the parsed values, or default values if
    /// the MarkInfo dictionary is not present.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use pdf_oxide::document::PdfDocument;
    /// # let mut doc = PdfDocument::open("sample.pdf")?;
    /// let mark_info = doc.mark_info()?;
    /// if mark_info.is_structure_reliable() {
    ///     println!("Structure tree can be trusted for reading order");
    /// } else if mark_info.suspects {
    ///     println!("Structure tree may contain unreliable content");
    /// }
    /// # Ok::<(), pdf_oxide::error::Error>(())
    /// ```
    pub fn mark_info(&self) -> Result<crate::structure::MarkInfo> {
        let catalog = self.catalog()?;
        let catalog_dict = match catalog.as_dict() {
            Some(d) => d,
            None => return Ok(crate::structure::MarkInfo::default()),
        };

        // Get /MarkInfo dictionary
        let mark_info_obj = match catalog_dict.get("MarkInfo") {
            Some(obj) => obj,
            None => return Ok(crate::structure::MarkInfo::default()),
        };

        // Resolve reference if needed
        let mark_info_obj = if let Some(r) = mark_info_obj.as_reference() {
            self.load_object(r)?
        } else {
            mark_info_obj.clone()
        };

        let mark_info_dict = match mark_info_obj.as_dict() {
            Some(d) => d,
            None => return Ok(crate::structure::MarkInfo::default()),
        };

        // Parse boolean fields with defaults of false
        let marked = mark_info_dict
            .get("Marked")
            .and_then(|o: &crate::object::Object| o.as_bool())
            .unwrap_or(false);

        let suspects = mark_info_dict
            .get("Suspects")
            .and_then(|o: &crate::object::Object| o.as_bool())
            .unwrap_or(false);

        let user_properties = mark_info_dict
            .get("UserProperties")
            .and_then(|o: &crate::object::Object| o.as_bool())
            .unwrap_or(false);

        Ok(crate::structure::MarkInfo {
            marked,
            suspects,
            user_properties,
        })
    }

    /// Get the number of pages in the document.
    ///
    /// This function:
    /// 1. Loads the catalog (root object)
    /// 2. Follows the /Pages reference to the page tree root
    /// 3. Extracts the /Count value from the page tree
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The catalog cannot be loaded
    /// - The /Pages entry is missing or invalid
    /// - The page tree root does not contain a /Count entry
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use pdf_oxide::document::PdfDocument;
    /// # let mut doc = PdfDocument::open("sample.pdf")?;
    /// let count = doc.page_count()?;
    /// println!("Document has {} pages", count);
    /// # Ok::<(), pdf_oxide::error::Error>(())
    /// ```
    /// Drop the per-page span, character, and content caches.
    ///
    /// These exist to avoid redoing work when a page is touched more than once. A
    /// forward-only page walk never touches one twice, so keeping them past a committed
    /// page just holds budget.
    pub fn release_page_scratch(&self) {
        self.page_spans_cache.lock_or_recover().clear();
        self.page_chars_cache.lock_or_recover().clear();
        self.page_content_cache.lock_or_recover().clear();
    }

    pub fn page_count(&self) -> Result<usize> {
        // Standard /Count reader, then a manual page-tree scan on failure.
        let primary: Result<usize> = match self.get_page_count_standard() {
            Ok(count) => {
                log::debug!("Page count from /Count: {}", count);
                Ok(count)
            }
            Err(Error::EncryptedPdf) => return Err(Error::EncryptedPdf),
            Err(e) => {
                // For encrypted PDFs any failure to read the page tree means we
                // cannot access the content, so surface it immediately.
                if self.is_encrypted() {
                    log::warn!("Page count failed for encrypted PDF: {}", e);
                    return Err(Error::EncryptedPdf);
                }
                log::warn!("Failed to get page count from /Count: {}", e);
                log::info!("Falling back to scanning page tree");
                match self.get_page_count_by_scanning() {
                    Ok(count) => {
                        log::info!("Page count from scanning: {}", count);
                        Ok(count)
                    }
                    Err(scan_err) => {
                        log::error!("Both methods failed. Standard: {}, Scan: {}", e, scan_err);
                        Err(e) // Return original error
                    }
                }
            }
        };

        // Enumerator rescue. A count of 0 from the /Count-based readers on a
        // non-encrypted document is almost always a page tree they could not
        // resolve - `/Pages` packed inside an object stream, or a deeply nested
        // `/Pages` -> `/Pages` -> `/Page` tree - not a genuinely empty document.
        // The /Count readers and `all_page_refs` (which walks `/Pages` -> `/Kids`
        // via `collect_page_refs`) both MISS such a tree; `get_page` still reaches
        // every page through its own per-page traversal / `collect_all_pages` bulk
        // walk, so count by agreeing with what it can actually reach. Gated on a
        // primary result of 0, so every document the standard reader counts
        // normally is unchanged.
        if matches!(primary, Ok(0)) && !self.is_encrypted() {
            // The /Count readers - and `all_page_refs`, which walks
            // `/Pages` -> `/Kids` via `collect_page_refs` - miss a page tree
            // packed inside an object stream. `get_page` still resolves every
            // such page through its own per-page traversal / `collect_all_pages`
            // bulk walk, so count by probing it: the definitive agreement with
            // the pages the rest of the API can actually reach. `get_page` never
            // calls back into `page_count` (no recursion) and caches each page
            // (repeat probes are cheap). For an ObjStm-packed tree each `get_page`
            // can fall back to a full object scan, so counting this way is
            // O(n * objects) - bounded by the sanity cap, and only ever on an
            // already-broken document. Only runs when the primary count is 0, so
            // normally-counted documents are byte-identical.
            let mut n = 0usize;
            while n < 1_000_000 && self.get_page(n).is_ok() {
                n += 1;
            }
            if n > 0 {
                log::info!("Page /Count was 0; enumerated {} pages via get_page", n);
                return Ok(n);
            }
        }
        primary
    }

    /// Get the MediaBox of a page (v0.3.14).
    ///
    /// MediaBox defines the physical boundaries of the page in user space units.
    pub fn get_page_media_box(&self, page_index: usize) -> Result<(f32, f32, f32, f32)> {
        let page = self.get_page(page_index)?;
        let page_dict = page
            .as_dict()
            .ok_or_else(|| Error::InvalidPdf("Page is not a dictionary".to_string()))?;

        // Resolve indirect reference if present — PDF spec §7.3.10 permits any value
        // to be an indirect reference, e.g. `/MediaBox 174 0 R` where 174 0 R is `[0 0 612 792]`.
        let media_box_obj_raw = page_dict
            .get("MediaBox")
            .ok_or_else(|| Error::InvalidPdf("MediaBox not found or not an array".to_string()))?;
        let media_box_obj = self.resolve_obj_ref(media_box_obj_raw);
        let media_box = media_box_obj
            .as_array()
            .ok_or_else(|| Error::InvalidPdf("MediaBox not found or not an array".to_string()))?;

        if media_box.len() < 4 {
            return Err(Error::InvalidPdf(
                "MediaBox must have at least 4 elements".to_string(),
            ));
        }

        fn to_f32(obj: &Object) -> f32 {
            match obj {
                Object::Integer(v) => *v as f32,
                Object::Real(v) => *v as f32,
                _ => 0.0,
            }
        }

        // §7.3.10: *any* element of the rectangle array may itself be an
        // indirect reference (pdf.js issue7872 stores `/MediaBox
        // [4 0 R 5 0 R 6 0 R 7 0 R]`). Resolve each element before
        // coercing — otherwise an unresolved Reference reads as 0.0 and
        // the page collapses to a zero-area box that clips all content.
        Ok((
            to_f32(&self.resolve_obj_ref(&media_box[0])),
            to_f32(&self.resolve_obj_ref(&media_box[1])),
            to_f32(&self.resolve_obj_ref(&media_box[2])),
            to_f32(&self.resolve_obj_ref(&media_box[3])),
        ))
    }

    /// Page `/Rotate` normalised to one of `{0, 90, 180, 270}`
    /// (ISO 32000-1 §7.7.3.3); `0` when absent or invalid.
    ///
    /// Pure inspection (no feature gate) for the auto-extraction
    /// classifier (#517 case I — transformed-bbox coverage / OCR
    /// orientation). Resolves via [`get_page`](Self::get_page), so the
    /// inheritable `/Rotate` attribute (ISO 32000-1 §7.7.3.4) is walked
    /// up the page tree — a `/Rotate` set on an ancestor `/Pages` node
    /// is honoured, not just one on the leaf page object.
    pub fn get_page_rotation(&self, page_index: usize) -> Result<i32> {
        let page = self.get_page(page_index)?;
        let dict = page
            .as_dict()
            .ok_or_else(|| Error::InvalidPdf("Page is not a dictionary".to_string()))?;
        let raw = match dict.get("Rotate") {
            Some(r) => match self.resolve_obj_ref(r) {
                Object::Integer(v) => v as i32,
                Object::Real(v) => v as i32,
                _ => 0,
            },
            None => 0,
        };
        // `/Rotate` shall be a multiple of 90 (ISO 32000-1 §7.7.3.3);
        // a non-multiple is invalid → `0` (per this fn's contract),
        // NOT silently floored (e.g. 135 must not become 90).
        let n = ((raw % 360) + 360) % 360;
        Ok(if n % 90 == 0 { n } else { 0 })
    }

    /// Get page count using the standard /Count field
    pub(super) fn get_page_count_standard(&self) -> Result<usize> {
        // Load catalog
        let catalog = self.catalog()?;
        let catalog_dict = catalog.as_dict().ok_or_else(|| Error::InvalidObjectType {
            expected: "Dictionary".to_string(),
            found: catalog.type_name().to_string(),
        })?;

        // Get /Pages reference
        let pages_ref = catalog_dict
            .get("Pages")
            .ok_or_else(|| Error::InvalidPdf("Catalog missing /Pages entry".to_string()))?
            .as_reference()
            .ok_or_else(|| Error::InvalidPdf("/Pages is not a reference".to_string()))?;

        // Load page tree root
        let pages_obj = self.load_object(pages_ref)?;
        let pages_dict = match pages_obj.as_dict() {
            Some(d) => d,
            None => {
                // If the page tree root resolved to Null it usually means the
                // PDF is encrypted and the page tree could not be decrypted.
                // Surface the real error instead of silently reporting 0 pages.
                if matches!(pages_obj, crate::object::Object::Null) && self.is_encrypted() {
                    return Err(Error::EncryptedPdf);
                }
                log::warn!(
                    "Page tree root is {} (expected Dictionary), treating as 0 pages",
                    pages_obj.type_name()
                );
                return Ok(0);
            }
        };

        // Get /Count
        let count = pages_dict
            .get("Count")
            .ok_or_else(|| Error::InvalidPdf("Page tree missing /Count entry".to_string()))?
            .as_integer()
            .ok_or_else(|| Error::InvalidPdf("/Count is not an integer".to_string()))?;

        // Validate /Count against PDF spec limits (Annex C.2: max 8,388,607 indirect objects)
        const MAX_PAGES: i64 = 8_388_607;
        if !(0..=MAX_PAGES).contains(&count) {
            log::warn!(
                "/Count value {} is unreasonable (max {}), falling back to tree scan",
                count,
                MAX_PAGES
            );
            return self.get_page_count_by_scanning();
        }

        // Sanity check: /Count can't exceed total objects in the file
        let max_objects = self.xref.len();
        if (count as usize) > max_objects {
            log::warn!(
                "/Count {} exceeds total objects {}, falling back to tree scan",
                count,
                max_objects
            );
            return self.get_page_count_by_scanning();
        }

        Ok(count as usize)
    }

    /// Get page count by scanning the page tree (fallback method)
    pub(super) fn get_page_count_by_scanning(&self) -> Result<usize> {
        // Load catalog
        let catalog = self.catalog()?;
        let catalog_dict = catalog.as_dict().ok_or_else(|| Error::InvalidObjectType {
            expected: "Dictionary".to_string(),
            found: catalog.type_name().to_string(),
        })?;

        // Get /Pages reference
        let pages_ref = catalog_dict
            .get("Pages")
            .ok_or_else(|| Error::InvalidPdf("Catalog missing /Pages entry".to_string()))?
            .as_reference()
            .ok_or_else(|| Error::InvalidPdf("/Pages is not a reference".to_string()))?;

        // Count pages by traversing the tree
        self.count_pages_recursive(pages_ref, 0)
    }

    /// Recursively count pages in the page tree
    pub(super) fn count_pages_recursive(&self, node_ref: ObjectRef, depth: usize) -> Result<usize> {
        // Prevent infinite recursion
        const MAX_DEPTH: usize = 50;
        if depth > MAX_DEPTH {
            log::warn!("Page tree depth exceeded {} levels, stopping", MAX_DEPTH);
            return Ok(0);
        }

        // Load the node
        let node = match self.load_object(node_ref) {
            Ok(n) => n,
            Err(e) => {
                log::warn!("Failed to load page tree node {}: {}", node_ref, e);
                return Ok(0); // Skip this node
            }
        };

        let node_dict = match node.as_dict() {
            Some(d) => d,
            None => {
                log::warn!("Page tree node {} is not a dictionary", node_ref);
                return Ok(0);
            }
        };

        // Check node type
        let node_type = node_dict.get("Type").and_then(|obj| obj.as_name());

        match node_type {
            Some("Page") => {
                // This is a leaf page
                Ok(1)
            }
            Some("Pages") => {
                // This is an intermediate node with kids
                let kids = match node_dict.get("Kids").and_then(|obj| obj.as_array()) {
                    Some(k) => k,
                    None => {
                        log::warn!("Pages node {} missing /Kids array", node_ref);
                        return Ok(0);
                    }
                };

                let mut count = 0;
                for kid in kids {
                    if let Some(kid_ref) = kid.as_reference() {
                        match self.count_pages_recursive(kid_ref, depth + 1) {
                            Ok(page_count) => count += page_count,
                            Err(Error::CircularReference(obj_ref)) => {
                                log::warn!(
                                    "Circular reference in page tree at object {}, skipping",
                                    obj_ref
                                );
                                continue;
                            }
                            Err(Error::RecursionLimitExceeded(_)) => {
                                log::warn!(
                                    "Recursion limit exceeded in page tree, skipping branch"
                                );
                                continue;
                            }
                            Err(e) => {
                                log::warn!("Error counting pages in branch: {}, skipping", e);
                                continue;
                            }
                        }
                    }
                }
                Ok(count)
            }
            _ => {
                log::warn!(
                    "Unknown page tree node type: {:?}",
                    node_type.unwrap_or("(none)")
                );
                Ok(0)
            }
        }
    }

    /// Get page count as u32 (legacy API).
    ///
    /// This is a convenience method that returns the page count as a u32.
    /// It calls `page_count()` internally but converts the result
    /// returns 0 if an error occurs (for backward compatibility).
    #[deprecated(
        since = "0.1.0",
        note = "Use page_count() instead, which returns Result"
    )]
    pub fn page_count_u32(&self) -> u32 {
        self.page_count().unwrap_or(0) as u32
    }
}
