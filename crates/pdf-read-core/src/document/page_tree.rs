use super::parsing::*;
use super::preflight::*;
use super::*;

impl PdfDocument {
    /// Returns the page index range `0..page_count`, or an empty range
    /// when `page_count()` fails. Issue #447.
    ///
    /// Designed for `for i in doc.page_indices() { ... }` so callers
    /// don't have to write `for i in 0..doc.page_count()?`. The
    /// fallible-vs-iterator tension that motivated the issue is
    /// resolved by treating a metadata-broken document as having no
    /// pages at the iteration level — every per-page extraction call
    /// is already fallible and surfaces the real error.
    ///
    /// # Example
    ///
    /// ```ignore
    /// for i in doc.page_indices() {
    ///     let text = doc.extract_text(i)?;
    ///     println!("page {}: {} chars", i, text.len());
    /// }
    /// ```
    pub fn page_indices(&self) -> std::ops::Range<usize> {
        0..self.page_count().unwrap_or(0)
    }

    /// Get a page object by index (0-based).
    ///
    /// # Arguments
    ///
    /// * `page_index` - Zero-based page index
    ///
    /// # Returns
    ///
    /// The page dictionary object.
    ///
    /// # Errors
    ///
    /// Returns an error if the page index is out of bounds or if the page
    /// tree structure is invalid.
    pub fn get_page(&self, page_index: usize) -> Result<Object> {
        // Check page cache first — page tree is static per §7.7.3.2
        if let Some(cached) = self.page_cache.lock_or_recover().get(&page_index).cloned() {
            return Ok(cached);
        }

        // Defer bulk page tree walk until enough pages are accessed.
        const LAZY_THRESHOLD: usize = 64;
        let cache_misses = self.page_cache.lock_or_recover().len();

        if !self.page_cache_populated.load(Ordering::Acquire) && cache_misses >= LAZY_THRESHOLD {
            self.page_cache_populated.store(true, Ordering::Release);
            if let Err(e) = self.populate_page_cache() {
                log::warn!(
                    "Bulk page tree walk failed ({}), falling back to per-page traversal",
                    e
                );
            }
            // Check cache after bulk population
            if let Some(cached) = self.page_cache.lock_or_recover().get(&page_index).cloned() {
                return Ok(cached);
            }
        }

        // Per-page tree traversal: walks only the branches needed to find target page
        let catalog = self.catalog()?;
        let catalog_dict = catalog.as_dict().ok_or_else(|| Error::InvalidObjectType {
            expected: "Dictionary".to_string(),
            found: catalog.type_name().to_string(),
        })?;

        let pages_ref = catalog_dict
            .get("Pages")
            .ok_or_else(|| Error::InvalidPdf("Catalog missing /Pages entry".to_string()))?
            .as_reference()
            .ok_or_else(|| Error::InvalidPdf("/Pages is not a reference".to_string()))?;

        let mut inherited = HashMap::new();

        let page = match self.get_page_from_tree(pages_ref, page_index, &mut 0, &mut inherited) {
            Ok(page) => {
                if let Some(dict) = page.as_dict() {
                    log::debug!("Collected page {}, keys: {:?}", page_index, dict.keys());
                    if let Some(contents) = dict.get("Contents") {
                        log::debug!("  -> /Contents: {:?}", contents);
                    }
                    if let Some(rotate) = dict.get("Rotate") {
                        log::debug!("  -> /Rotate: {:?}", rotate);
                    }
                }
                Ok(page)
            }
            Err(e) => {
                if matches!(
                    e,
                    Error::InvalidPdf(_)
                        | Error::InvalidObjectType { .. }
                        | Error::CircularReference(_)
                        | Error::ObjectNotFound(_, _)
                ) {
                    log::warn!(
                        "Page tree traversal failed ({}), trying fallback scan method",
                        e
                    );
                    self.get_page_by_scanning(page_index)
                } else {
                    Err(e)
                }
            }
        }?;

        self.page_cache
            .lock_or_recover()
            .insert(page_index, page.clone());
        Ok(page)
    }

    /// Walk the page tree once and populate page_cache for ALL pages.
    /// This avoids O(n²) cost when pages are accessed sequentially.
    pub(super) fn populate_page_cache(&self) -> Result<()> {
        let catalog = self.catalog()?;
        let catalog_dict = catalog.as_dict().ok_or_else(|| Error::InvalidObjectType {
            expected: "Dictionary".to_string(),
            found: catalog.type_name().to_string(),
        })?;

        let pages_ref = catalog_dict
            .get("Pages")
            .ok_or_else(|| Error::InvalidPdf("Catalog missing /Pages entry".to_string()))?
            .as_reference()
            .ok_or_else(|| Error::InvalidPdf("/Pages is not a reference".to_string()))?;

        let mut page_index = 0usize;
        let mut inherited = HashMap::new();
        self.collect_all_pages(
            pages_ref,
            &mut page_index,
            &mut inherited,
            &mut HashSet::new(),
        )?;
        log::debug!("Populated page cache with {} pages", page_index);
        Ok(())
    }

    /// Pre-populate `image_xobject_cache` for all XObject refs across all cached pages.
    /// Collects all unique XObject references, sorts them by xref offset for sequential
    /// I/O (avoids random seeking in large files), then peeks each one via `is_form_xobject()`.
    #[allow(dead_code)]
    pub(super) fn prefetch_xobject_subtypes(&self) {
        // Collect all unique XObject refs from all cached pages
        let mut xobj_refs: Vec<ObjectRef> = Vec::new();
        let page_dicts: Vec<Object> = self
            .page_cache
            .lock_or_recover()
            .values()
            .cloned()
            .collect();

        for page_obj in &page_dicts {
            let page_dict = match page_obj.as_dict() {
                Some(d) => d,
                None => continue,
            };
            let resources = match page_dict.get("Resources") {
                Some(r) => {
                    if let Some(ref_obj) = r.as_reference() {
                        match self.load_object(ref_obj) {
                            Ok(obj) => obj,
                            Err(_) => continue,
                        }
                    } else {
                        r.clone()
                    }
                }
                None => continue,
            };
            let res_dict = match resources.as_dict() {
                Some(d) => d,
                None => continue,
            };
            let xobj_obj = match res_dict.get("XObject") {
                Some(x) => {
                    if let Some(ref_obj) = x.as_reference() {
                        match self.load_object(ref_obj) {
                            Ok(obj) => obj,
                            Err(_) => continue,
                        }
                    } else {
                        x.clone()
                    }
                }
                None => continue,
            };
            if let Some(xobj_dict) = xobj_obj.as_dict() {
                for val in xobj_dict.values() {
                    if let Some(obj_ref) = val.as_reference() {
                        if !self
                            .image_xobject_cache
                            .lock_or_recover()
                            .contains(&obj_ref)
                        {
                            xobj_refs.push(obj_ref);
                        }
                    }
                }
            }
        }

        // Deduplicate
        xobj_refs.sort_unstable_by_key(|r| (r.id, r.gen));
        xobj_refs.dedup();

        // Sort by xref offset for sequential I/O
        xobj_refs.sort_by_key(|r| self.xref.get(r.id).map(|e| e.offset).unwrap_or(u64::MAX));

        log::debug!(
            "Prefetching XObject subtypes for {} unique refs",
            xobj_refs.len()
        );

        // Peek each ref — populates image_xobject_cache as a side effect
        for obj_ref in xobj_refs {
            self.is_form_xobject(obj_ref);
        }
    }

    /// Recursively walk the page tree and collect all pages into page_cache.
    pub(super) fn collect_all_pages(
        &self,
        node_ref: ObjectRef,
        page_index: &mut usize,
        inherited: &mut HashMap<String, Object>,
        visited: &mut HashSet<ObjectRef>,
    ) -> Result<()> {
        if !visited.insert(node_ref) {
            return Err(Error::CircularReference(node_ref));
        }

        let node = self.load_object(node_ref)?;
        let node_dict = match node.as_dict() {
            Some(d) => d,
            None => return Ok(()), // Skip non-dict nodes gracefully
        };

        let node_type = node_dict
            .get("Type")
            .and_then(|obj| obj.as_name())
            .unwrap_or("");

        match node_type {
            "Page" => {
                // Apply inherited attributes
                let mut page_dict = node_dict.clone();
                for attr_name in &["Resources", "MediaBox", "CropBox", "Rotate"] {
                    if !page_dict.contains_key(*attr_name) {
                        if let Some(inherited_value) = inherited.get(*attr_name) {
                            log::debug!(
                                "Page {} inheriting {}: {:?}",
                                *page_index,
                                attr_name,
                                inherited_value
                            );
                            page_dict.insert(attr_name.to_string(), inherited_value.clone());
                        }
                    }
                }
                log::debug!(
                    "Collected page {}, keys: {:?}",
                    *page_index,
                    page_dict.keys()
                );
                if let Some(contents) = page_dict.get("Contents") {
                    log::debug!("  -> /Contents: {:?}", contents);
                }
                if let Some(rotate) = page_dict.get("Rotate") {
                    log::debug!("  -> /Rotate: {:?}", rotate);
                }
                self.page_cache
                    .lock_or_recover()
                    .insert(*page_index, Object::Dictionary(page_dict));
                *page_index += 1;
            }
            "Pages" => {
                // Save inherited state so siblings don't see each other's overrides
                let saved = inherited.clone();

                // Nearest ancestor's attributes override more distant ones (PDF spec §7.7.3.4).
                // insert() is correct here because we snapshot/restore `inherited` around
                // the recursion, so this node's values apply only to its subtree.
                for attr_name in &["Resources", "MediaBox", "CropBox", "Rotate"] {
                    if let Some(attr_value) = node_dict.get(*attr_name) {
                        log::debug!(
                            "Pages node at {:?} providing inheritable {}: {:?}",
                            node_ref,
                            attr_name,
                            attr_value
                        );
                        inherited.insert(attr_name.to_string(), attr_value.clone());
                    }
                }

                if let Some(kids) = node_dict.get("Kids").and_then(|obj| obj.as_array()) {
                    for kid in kids {
                        if let Some(kid_ref) = kid.as_reference() {
                            if let Err(e) =
                                self.collect_all_pages(kid_ref, page_index, inherited, visited)
                            {
                                log::warn!(
                                    "Error collecting page from tree: {}, skipping branch",
                                    e
                                );
                            }
                        }
                    }
                }

                *inherited = saved;
            }
            _ => {} // Unknown node type, skip
        }

        Ok(())
    }

    /// Get a page by scanning all objects in the PDF (fallback for broken page trees)
    /// This method is used when the standard page tree traversal fails due to malformed structure.
    pub(super) fn get_page_by_scanning(&self, target_index: usize) -> Result<Object> {
        let mut current_index = 0;

        // Prime the ObjStm recovery cache up front when the xref looks
        // unreliable. Without this, the first pass below iterates only
        // `xref.all_object_numbers()` — which misses compressed objects
        // whose xref slots have been mis-flagged free. The sweep is a
        // one-shot, guarded by `objstm_recovery_done`, so this is cheap
        // if recovery already happened.
        self.recover_from_object_streams();

        // Collect all object numbers first to avoid borrow checker issues.
        // Sort for deterministic iteration order (HashMap iteration is
        // non-deterministic). We union the xref-listed ids with the object
        // ids recovered from the ObjStm sweep so that pages compressed in
        // streams whose xref slots were mis-flagged free still get visited.
        let mut obj_nums: Vec<u32> = self.xref.all_object_numbers().collect();
        for r in self.object_cache.lock_or_recover().keys() {
            obj_nums.push(r.id);
        }
        obj_nums.sort_unstable();
        obj_nums.dedup();

        // First pass: look for objects with /Type /Page
        for &obj_num in &obj_nums {
            if let Ok(obj) = self.load_object(ObjectRef {
                id: obj_num,
                gen: 0,
            }) {
                if let Some(dict) = obj.as_dict() {
                    if let Some(type_obj) = dict.get("Type") {
                        if let Some(type_name) = type_obj.as_name() {
                            if type_name == "Page" {
                                if current_index == target_index {
                                    return Ok(obj);
                                }
                                current_index += 1;
                            }
                        }
                    }
                }
            }
        }

        // Second pass: heuristic detection for pages without /Type entry.
        // Runs as a complement to pass 1 — counts page-like dicts that lack
        // a /Type entry alongside the /Type /Page matches, so that PDFs
        // whose corruption stripped /Type from some page dicts still reach
        // the full page count. Previously this pass only ran when pass 1
        // found zero pages, which meant any partial pass-1 match (e.g. 200
        // of 253 pages) would silently short pass 2 and fail.
        let mut heuristic_index = current_index;
        for &obj_num in &obj_nums {
            if let Ok(obj) = self.load_object(ObjectRef {
                id: obj_num,
                gen: 0,
            }) {
                if let Some(dict) = obj.as_dict() {
                    let has_no_type = dict.get("Type").is_none();
                    // Also handle /Type that is an unresolvable reference (Null)
                    let type_is_null = dict.get("Type").is_some_and(|t| matches!(t, Object::Null));
                    if (has_no_type || type_is_null)
                        && (dict.contains_key("MediaBox")
                            || dict.contains_key("Contents")
                            || (dict.contains_key("Resources") && dict.contains_key("Parent")))
                    {
                        log::debug!(
                            "Heuristic page candidate: object {} (page-like keys without valid /Type)",
                            obj_num
                        );
                        if heuristic_index == target_index {
                            return Ok(obj);
                        }
                        heuristic_index += 1;
                    }
                }
            }
        }
        current_index = heuristic_index;

        // Third pass: try resolving /Kids from catalog's /Pages root directly
        if current_index == 0 {
            if let Ok(catalog) = self.catalog() {
                if let Some(catalog_dict) = catalog.as_dict() {
                    if let Some(pages_ref) =
                        catalog_dict.get("Pages").and_then(|p| p.as_reference())
                    {
                        if let Ok(pages_obj) = self.load_object(pages_ref) {
                            if let Some(pages_dict) = pages_obj.as_dict() {
                                if let Some(kids) =
                                    pages_dict.get("Kids").and_then(|k| k.as_array())
                                {
                                    let mut kids_index = 0;
                                    for kid in kids {
                                        if let Some(kid_ref) = kid.as_reference() {
                                            // Skip self-referencing kids (cycle detection)
                                            if kid_ref == pages_ref {
                                                continue;
                                            }
                                            if let Ok(kid_obj) = self.load_object(kid_ref) {
                                                if let Some(kid_dict) = kid_obj.as_dict() {
                                                    // Skip intermediate /Pages nodes
                                                    let is_pages_node = kid_dict
                                                        .get("Type")
                                                        .and_then(|t| t.as_name())
                                                        .is_some_and(|n| n == "Pages");
                                                    if is_pages_node {
                                                        continue;
                                                    }
                                                    if kids_index == target_index {
                                                        log::debug!("Found page {} via direct /Kids resolution of object {}", target_index, kid_ref.id);
                                                        return Ok(kid_obj);
                                                    }
                                                    kids_index += 1;
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        Err(Error::InvalidPdf(format!(
            "Page index {} not found by scanning",
            target_index
        )))
    }

    /// Recursively traverse page tree to find a specific page.
    ///
    /// PDF Spec: ISO 32000-1:2008, Section 7.7.3.3 - Page Objects
    /// Implements attribute inheritance for /Resources, /MediaBox, /CropBox, /Rotate.
    ///
    /// Inheritable attributes from parent Pages nodes are collected as we traverse down
    /// the tree. When a Page is found, inherited attributes are merged in (only if the
    /// Page doesn't already have them - child values override parent values).
    pub(super) fn get_page_from_tree(
        &self,
        node_ref: ObjectRef,
        target_index: usize,
        current_index: &mut usize,
        inherited: &mut HashMap<String, Object>,
    ) -> Result<Object> {
        self.get_page_from_tree_inner(
            node_ref,
            target_index,
            current_index,
            inherited,
            &mut HashSet::new(),
        )
    }

    pub(super) fn get_page_from_tree_inner(
        &self,
        node_ref: ObjectRef,
        target_index: usize,
        current_index: &mut usize,
        inherited: &mut HashMap<String, Object>,
        visited: &mut HashSet<ObjectRef>,
    ) -> Result<Object> {
        if !visited.insert(node_ref) {
            return Err(Error::CircularReference(node_ref));
        }
        let node = self.load_object(node_ref)?;
        let node_dict = match node.as_dict() {
            Some(d) => d,
            None => {
                // Null or non-dict node in page tree — skip it
                log::warn!(
                    "Page tree node {} is {} (expected Dictionary), skipping",
                    node_ref.id,
                    node.type_name()
                );
                return Err(Error::InvalidPdf(format!(
                    "Page tree node {} is not a dictionary",
                    node_ref.id
                )));
            }
        };

        // Check if this is a page or pages node
        let node_type = node_dict
            .get("Type")
            .and_then(|obj| obj.as_name())
            .ok_or_else(|| Error::InvalidPdf("Page tree node missing /Type".to_string()))?;

        match node_type {
            "Pages" if *current_index < target_index => {
                // Skip entire subtree if /Count shows target is past this node.
                if let Some(count) = node_dict
                    .get("Count")
                    .and_then(|c| c.as_integer())
                    .filter(|&c| c > 0)
                {
                    let count = count as usize;
                    if *current_index + count <= target_index {
                        *current_index += count;
                        return Err(Error::InvalidPdf(format!(
                            "Page index {} not found in tree",
                            target_index
                        )));
                    }
                }
            }
            _ => {}
        }

        match node_type {
            "Page" => {
                if *current_index == target_index {
                    // Apply inherited attributes to this page
                    // PDF Spec: "If not present in the page dictionary, the value is inherited
                    // from an ancestor node in the page tree"
                    let mut page_dict = node_dict.clone();

                    // Inheritable attributes per PDF Spec Table 30:
                    // - Resources (required, can be inherited)
                    // - MediaBox (required, can be inherited)
                    // - CropBox (optional, can be inherited)
                    // - Rotate (optional, can be inherited)
                    let inheritable_attrs = ["Resources", "MediaBox", "CropBox", "Rotate"];

                    for attr_name in &inheritable_attrs {
                        // Only inherit if page doesn't already have this attribute
                        if !page_dict.contains_key(*attr_name) {
                            if let Some(inherited_value) = inherited.get(*attr_name) {
                                log::debug!(
                                    "Page {} inheriting /{} from ancestor Pages node",
                                    target_index,
                                    attr_name
                                );
                                page_dict.insert(attr_name.to_string(), inherited_value.clone());
                            }
                        }
                    }

                    Ok(Object::Dictionary(page_dict))
                } else {
                    *current_index += 1;
                    Err(Error::InvalidPdf(format!(
                        "Page index {} not found in tree",
                        target_index
                    )))
                }
            }
            "Pages" => {
                // This is an intermediate Pages node with kids
                // Collect inheritable attributes from this node to pass to children
                let inheritable_attrs = ["Resources", "MediaBox", "CropBox", "Rotate"];

                for attr_name in &inheritable_attrs {
                    if let Some(attr_value) = node_dict.get(*attr_name) {
                        // Only add if not already in inherited map (child values override parent)
                        inherited
                            .entry(attr_name.to_string())
                            .or_insert_with(|| attr_value.clone());
                    }
                }

                // Try to get /Kids array; if missing, this is a malformed PDF
                let kids = match node_dict.get("Kids").and_then(|obj| obj.as_array()) {
                    Some(k) => k,
                    None => {
                        log::warn!("Malformed PDF: Pages node missing /Kids array");
                        // Malformed PDF: Pages node has no /Kids array
                        // Gracefully return without error to allow other recovery paths
                        // The scanning method will find pages eventually
                        return Err(Error::InvalidPdf(
                            "Pages node missing /Kids array - try fallback method".to_string(),
                        ));
                    }
                };

                for kid in kids {
                    let kid_ref = kid.as_reference().ok_or_else(|| {
                        Error::InvalidPdf("Kid in /Kids array is not a reference".to_string())
                    })?;

                    match self.get_page_from_tree_inner(
                        kid_ref,
                        target_index,
                        current_index,
                        inherited,
                        visited,
                    ) {
                        Ok(page) => return Ok(page),
                        Err(Error::CircularReference(obj_ref)) => {
                            log::warn!(
                                "Circular reference in page tree at object {}, skipping",
                                obj_ref
                            );
                            continue;
                        }
                        Err(Error::RecursionLimitExceeded(_)) => {
                            log::warn!("Recursion limit exceeded in page tree, skipping branch");
                            continue;
                        }
                        Err(_) => continue,
                    }
                }

                Err(Error::InvalidPdf(format!(
                    "Page index {} not found",
                    target_index
                )))
            }
            _ => Err(Error::InvalidPdf(format!(
                "Unknown page tree node type: {}",
                node_type
            ))),
        }
    }

    /// Get the object reference for a page by index.
    ///
    /// This is used by outline and annotations to find page references.
    pub(crate) fn get_page_ref(&self, page_index: usize) -> Result<ObjectRef> {
        let catalog = self.catalog()?;
        let catalog_dict = catalog.as_dict().ok_or_else(|| Error::InvalidObjectType {
            expected: "Dictionary".to_string(),
            found: catalog.type_name().to_string(),
        })?;

        let pages_ref = catalog_dict
            .get("Pages")
            .ok_or_else(|| Error::InvalidPdf("Catalog missing /Pages entry".to_string()))?
            .as_reference()
            .ok_or_else(|| Error::InvalidPdf("/Pages is not a reference".to_string()))?;

        self.get_page_ref_recursive(pages_ref, page_index, &mut 0, &mut HashSet::new())
    }
}
