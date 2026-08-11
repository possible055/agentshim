use super::parsing::*;
use super::preflight::*;
use super::*;

impl PdfDocument {
    /// Load fonts from a Resources dictionary into the extractor.
    pub(crate) fn load_fonts(
        &self,
        resources: &Object,
        extractor: &mut crate::extractors::TextExtractor<'_>,
    ) -> Result<()> {
        use crate::fonts::FontInfo;

        // Resources can be a reference or a dictionary
        let resources_obj = if let Some(res_ref) = resources.as_reference() {
            self.load_object(res_ref)?
        } else {
            resources.clone()
        };

        let resources_dict = match resources_obj.as_dict() {
            Some(d) => d,
            None => {
                log::warn!(
                    "Resources is not a dictionary (type: {}), treating as empty",
                    resources_obj.type_name()
                );
                return Ok(());
            }
        };

        // Get Font dictionary if present
        if let Some(font_obj) = resources_dict.get("Font") {
            // Font can be a reference or direct dictionary - need to dereference
            let font_dict_ref = font_obj.as_reference();
            let font_dict_obj = if let Some(font_ref) = font_dict_ref {
                self.load_object(font_ref)?
            } else {
                font_obj.clone()
            };

            // Layer 2: Check font set cache for the /Font dictionary.
            // Pages sharing the same /Font dict skip the entire per-font loop.
            if let Some(font_dict_ref) = font_dict_ref {
                let cached_set_opt = self
                    .font_set_cache
                    .lock_or_recover()
                    .get(&font_dict_ref)
                    .cloned();
                if let Some(cached_set) = cached_set_opt {
                    for (name, font_arc) in &cached_set {
                        extractor.add_font_shared(name.clone(), Arc::clone(font_arc));
                    }
                    extractor.share_truetype_cmaps();
                    return Ok(());
                }
            }

            if let Some(font_dict) = font_dict_obj.as_dict() {
                // Compute font fingerprint from (name → ObjectRef) pairs.
                // Hash the MAPPING between font names and their object refs,
                // not just the sets separately. This prevents false cache hits
                // when two font dicts have the same set of refs and names but
                // different name-to-ref assignments.
                let fingerprint = {
                    use std::hash::{Hash, Hasher};
                    let mut hasher = std::collections::hash_map::DefaultHasher::new();
                    let mut name_ref_pairs: Vec<(&str, Option<ObjectRef>)> = font_dict
                        .iter()
                        .map(|(name, fo)| (name.as_str(), fo.as_reference()))
                        .collect();
                    name_ref_pairs.sort_by(|a, b| a.0.cmp(b.0));
                    for (name, obj_ref) in &name_ref_pairs {
                        name.hash(&mut hasher);
                        if let Some(r) = obj_ref {
                            r.id.hash(&mut hasher);
                            r.gen.hash(&mut hasher);
                        }
                    }
                    hasher.finish()
                };

                let cached_fingerprint_opt = self
                    .font_fingerprint_cache
                    .lock_or_recover()
                    .get(&fingerprint)
                    .cloned();
                if let Some(cached_set) = cached_fingerprint_opt {
                    for (name, font_arc) in &cached_set {
                        extractor.add_font_shared(name.clone(), Arc::clone(font_arc));
                    }
                    extractor.share_truetype_cmaps();
                    return Ok(());
                }

                // Layer 4: Name-based font set cache with spot-check verification.
                // Pages in the same document often use the same font names mapped to
                // different ObjectRefs but identical base fonts (e.g., 764 pages each
                // creating T1_0→Helvetica, T1_1→Times-Roman with unique object numbers).
                // Cache the resolved font set by sorted font names, then on subsequent
                // pages verify ONE font via load+hash to confirm the mapping is the same.
                let name_hash = {
                    use std::hash::{Hash, Hasher};
                    let mut font_names: Vec<&str> = font_dict.keys().map(|k| k.as_str()).collect();
                    font_names.sort();
                    let mut hasher = std::collections::hash_map::DefaultHasher::new();
                    font_names.hash(&mut hasher);
                    hasher.finish()
                };

                let cached_name_set = self
                    .font_name_set_cache
                    .lock_or_recover()
                    .get(&name_hash)
                    .cloned();
                // Sort font entries by name for deterministic processing order.
                // HashMap iteration order is randomized per-process, which causes
                // non-deterministic text extraction when font CMap sharing depends
                // on the order fonts are loaded.
                let mut sorted_font_entries: Vec<(&String, &Object)> = font_dict.iter().collect();
                sorted_font_entries.sort_by_key(|(name, _)| name.as_str());

                if let Some((cached_set, check_hash)) = cached_name_set {
                    // Verify the cached font set by computing a combined identity hash
                    // over ALL reference fonts in the current Resources dict (sorted by
                    // name). This prevents false cache hits when pages reuse the same
                    // font key names but embed different per-page subsets — a single-font
                    // spot-check is insufficient because it only guards one entry
                    // lets differing sibling fonts (F2, F3 …) slip through unchecked.
                    // Fixes the regression described in issue #408.
                    let current_combined = {
                        use std::hash::{Hash, Hasher};
                        let mut h = std::collections::hash_map::DefaultHasher::new();
                        for (name, font_obj) in &sorted_font_entries {
                            if let Some(font_ref) = font_obj.as_reference() {
                                if let Some(fh) = self.cached_font_identity_hash(font_ref) {
                                    name.as_str().hash(&mut h);
                                    fh.hash(&mut h);
                                }
                            }
                        }
                        h.finish()
                    };
                    if current_combined == check_hash {
                        for (name, font_arc) in cached_set.iter() {
                            extractor.add_font_shared(name.clone(), Arc::clone(font_arc));
                        }
                        extractor.share_truetype_cmaps();
                        return Ok(());
                    }
                    // Hash mismatch: fonts differ — fall through to full load.
                }

                // Snapshot names already in the extractor before this load_fonts call.
                // Layer 4 must store only the delta so that a cache hit never injects
                // parent-page fonts into a different page's extractor context, which
                // would overwrite correctly-loaded fonts with wrong versions.
                let extractor_names_before: std::collections::HashSet<String> = extractor
                    .get_font_set()
                    .into_iter()
                    .map(|(k, _)| k)
                    .collect();

                let mut all_from_cache = true;

                for (name, font_obj) in &sorted_font_entries {
                    // If font is a reference, check per-font cache first
                    if let Some(font_ref) = font_obj.as_reference() {
                        let cached_font_opt =
                            self.font_cache.lock_or_recover().get(&font_ref).cloned();
                        if let Some(cached) = cached_font_opt {
                            extractor.add_font_shared((*name).clone(), cached);
                            continue;
                        }
                        all_from_cache = false;
                        let font = self.load_object(font_ref)?;

                        // Compute identity hash. For Type0 fonts this also
                        // resolves the descendant CIDFont and folds its
                        // /DW, /DW2, /W, /W2 into the key — otherwise two
                        // Type0 fonts whose top-level dicts have identical
                        // inline shape but whose CIDFonts ship different
                        // horizontal or vertical metrics would collide on
                        // the Layer 5/6 caches.
                        let id_hash = self.font_identity_hash_with_descendants(&font);

                        // Type 3 fonts and subset fonts must not cross
                        // PdfDocument boundaries via the global cache — their
                        // glyph procs / glyph-subset + ToUnicode mappings are
                        // document-specific. The per-document Layer 4/5 caches
                        // below stay safe to use.
                        let is_document_local = Self::font_is_document_local(&font);

                        // Layer 5: Per-font identity cache — skip from_dict when a
                        // structurally identical font was already parsed elsewhere.
                        let cached_identity_opt = self
                            .font_identity_cache
                            .lock_or_recover()
                            .get(&id_hash)
                            .cloned();
                        if let Some(cached) = cached_identity_opt {
                            self.font_cache
                                .lock_or_recover()
                                .insert(font_ref, Arc::clone(&cached));
                            extractor.add_font_shared((*name).clone(), cached);
                            continue;
                        }

                        // Layer 6: Global cross-document font cache — reuse fonts
                        // parsed by previous PdfDocument instances in this process.
                        // Skipped entirely for document-local fonts (#597).
                        if !is_document_local {
                            if let Some(cached) =
                                crate::fonts::global_cache::global_font_cache_get(id_hash)
                            {
                                self.font_identity_cache
                                    .lock_or_recover()
                                    .insert(id_hash, Arc::clone(&cached));
                                self.font_cache
                                    .lock_or_recover()
                                    .insert(font_ref, Arc::clone(&cached));
                                extractor.add_font_shared((*name).clone(), cached);
                                continue;
                            }
                        }

                        match FontInfo::from_dict(&font, self) {
                            Ok(font_info) => {
                                let arc = Arc::new(font_info);
                                // Populate the document-level caches always; the
                                // global cross-document cache only for fonts that
                                // are safe to share across documents (#597).
                                if !is_document_local {
                                    crate::fonts::global_cache::global_font_cache_insert(
                                        id_hash,
                                        Arc::clone(&arc),
                                    );
                                }
                                self.font_identity_cache
                                    .lock_or_recover()
                                    .insert(id_hash, Arc::clone(&arc));
                                self.font_cache
                                    .lock_or_recover()
                                    .insert(font_ref, Arc::clone(&arc));
                                extractor.add_font_shared((*name).clone(), arc);
                            }
                            Err(e) => {
                                log::error!(
                                    "Failed to load font '{}': {}. Text using this font will use fallback encoding.",
                                    name,
                                    e
                                );
                                continue;
                            }
                        }
                    } else {
                        // Direct font object — parse without caching (no stable key)
                        all_from_cache = false;
                        let font = *font_obj;
                        match FontInfo::from_dict(font, self) {
                            Ok(font_info) => {
                                extractor.add_font((*name).clone(), font_info);
                            }
                            Err(e) => {
                                log::error!(
                                    "Failed to load font '{}': {}. Text using this font will use fallback encoding.",
                                    name,
                                    e
                                );
                                continue;
                            }
                        }
                    }
                }

                // Always re-share TrueType CMaps after loading fonts. Cached fonts
                // may lack donated CMaps because Arc::make_mut creates a per-extractor
                // clone that is not written back to per-font cache. A donor font added
                // in a later load_fonts call (e.g. an XObject font donating to a
                // page-level font already in the extractor) requires sharing to run
                // again even when all fonts came from cache.
                extractor.share_truetype_cmaps();

                // Cache font set by both ObjectRef and fingerprint
                let font_set = extractor.get_font_set();
                if let Some(fdr) = font_dict_ref {
                    self.font_set_cache
                        .lock_or_recover()
                        .insert(fdr, font_set.clone());
                }
                self.font_fingerprint_cache
                    .lock_or_recover()
                    .insert(fingerprint, font_set.clone());

                // Cache by font names for Layer 4. Store only the delta — fonts
                // added by THIS load_fonts call — so that a cache hit never pollutes
                // a different page's extractor with stale parent-page fonts.
                // The combined identity hash covers ALL reference fonts (sorted by
                // name), so a hit requires every font in the Resources dict to match,
                // not just one. This prevents false positives when pages reuse the
                // same font key names with different per-page subsets.
                if !all_from_cache {
                    let combined_check_hash = {
                        use std::hash::{Hash, Hasher};
                        let mut h = std::collections::hash_map::DefaultHasher::new();
                        for (name, font_obj) in &sorted_font_entries {
                            if let Some(font_ref) = font_obj.as_reference() {
                                if let Some(fh) = self.cached_font_identity_hash(font_ref) {
                                    name.as_str().hash(&mut h);
                                    fh.hash(&mut h);
                                }
                            }
                        }
                        h.finish()
                    };
                    let l4_set: Vec<(String, Arc<FontInfo>)> = font_set
                        .iter()
                        .filter(|(k, _)| !extractor_names_before.contains(k.as_str()))
                        .map(|(k, v)| (k.clone(), Arc::clone(v)))
                        .collect();
                    self.font_name_set_cache
                        .lock_or_recover()
                        .insert(name_hash, (Arc::new(l4_set), combined_check_hash));
                }

                return Ok(());
            }
        }

        Ok(())
    }
}
