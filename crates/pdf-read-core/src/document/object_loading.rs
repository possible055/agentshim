use super::parsing::*;
use super::preflight::*;
use super::*;

impl PdfDocument {
    /// Scan the file to find an object by its header.
    ///
    /// This is a fallback method used when an object is not in the xref table
    /// but is referenced by critical structures (like Pages from Catalog).
    /// Some PDFs have incomplete xref tables that are missing entries for
    /// objects that actually exist in the file.
    pub(super) fn scan_for_object(&self, obj_ref: ObjectRef) -> Result<u64> {
        // Check cached scan results first
        {
            let scan_cache = self.scanned_object_offsets.lock_or_recover();
            if let Some(offsets) = scan_cache.as_ref() {
                if let Some(&offset) = offsets.get(&obj_ref.id) {
                    return Ok(offset);
                }
                return Err(Error::ObjectNotFound(obj_ref.id, obj_ref.gen));
            }
        }

        // First xref miss: scan the entire file once and build a complete offset map
        log::info!(
            "Building object offset map from file scan (triggered by object {} {})",
            obj_ref.id,
            obj_ref.gen
        );

        let mut offsets = HashMap::new();
        {
            // Hold one guard across the whole scan to prevent a split-lock race
            // (#398 Race A). The file is read in bounded windows rather than whole:
            // this fallback is triggered by an incomplete xref, so it is reachable from
            // ordinary input, and buffering the file here would make a second complete
            // copy of attacker-controlled data.
            let mut reader = self.reader.lock_or_recover();
            for chunk in crate::xref_reconstruction::ScanChunks::new(&mut *reader)? {
                let chunk = chunk?;
                let content = chunk.bytes.as_slice();
                scan_object_headers(content, &chunk, &mut offsets);
            }
        }
        log::info!("File scan found {} objects", offsets.len());

        let result = offsets.get(&obj_ref.id).copied();
        *self.scanned_object_offsets.lock_or_recover() = Some(offsets);

        match result {
            Some(offset) => Ok(offset),
            None => Err(Error::ObjectNotFound(obj_ref.id, obj_ref.gen)),
        }
    }

    /// One-time sweep over every known object stream (`/Type /ObjStm`),
    /// used to recover from xref tables that mis-mark compressed objects as
    /// free.
    ///
    /// Some PDF producers emit an xref where a compressed object's slot is
    /// type 0 (free) instead of type 2 (compressed → stream#). The object
    /// is physically stored inside an `ObjStm`, but `scan_for_object` can't
    /// find it because it has no standalone `N G obj` marker in the body.
    ///
    /// The recovery: iterate every uncompressed candidate, peek at the
    /// dictionary, and for those that are `/Type /ObjStm`, parse the stream
    /// and cache everything inside (overwriting any stale `Object::Null`
    /// entries from earlier free-entry short-circuits).
    ///
    /// Runs at most once per document — guarded by `objstm_recovery_done`.
    /// Cost is amortised across every recovered object.
    pub(super) fn recover_from_object_streams(&self) {
        use crate::objstm::parse_object_stream_with_decryption;

        {
            let done = self.objstm_recovery_done.lock_or_recover();
            if *done {
                return;
            }
        }

        log::debug!("Sweeping object streams to recover xref-flagged-free objects");

        // Find ObjStm candidates by raw pattern search in the file body.
        //
        // Why not iterate xref entries here: the xref is precisely what we
        // don't trust in this recovery path — its offsets may be wrong
        // its type tags may be lying about what each slot contains. A raw
        // search for `N G obj ... /Type /ObjStm` finds every object stream
        // the producer actually wrote, independent of how the xref
        // describes them.
        //
        // Only flip `objstm_recovery_done` after we finish the scan+parse
        // pass; a transient seek/read failure should leave the flag unset
        // so a later retry can still attempt recovery.
        // Windowed rather than whole-file: this recovery is reached from ordinary input
        // (an xref that mis-marks compressed objects as free), so buffering the file here
        // would make a second complete copy of attacker-controlled data. The overlap
        // covers the dictionary peek so a header near a window edge still sees its
        // `/Type /ObjStm`.
        let candidates = {
            let mut r = self.reader.lock_or_recover();
            let chunks = match crate::xref_reconstruction::ScanChunks::with_overlap(
                &mut *r,
                OBJSTM_SCAN_OVERLAP_BYTES,
            ) {
                Ok(chunks) => chunks,
                Err(_) => return,
            };
            let mut candidates = Vec::new();
            for chunk in chunks {
                let Ok(chunk) = chunk else {
                    return;
                };
                candidates.extend(find_objstm_candidates(chunk.bytes.as_slice(), &chunk));
            }
            candidates
        };

        let mut objstms_found = 0usize;
        let mut recovered = 0usize;
        for (stream_obj_num, offset) in &candidates {
            let stream_ref = ObjectRef::new(*stream_obj_num, 0);
            let stream_obj = match self.load_uncompressed_object(stream_ref, *offset) {
                Ok(obj) => obj,
                Err(_) => continue,
            };

            let is_objstm = stream_obj
                .as_dict()
                .and_then(|d| d.get("Type"))
                .and_then(|t| t.as_name())
                .is_some_and(|n| n == "ObjStm");
            if !is_objstm {
                continue;
            }
            objstms_found += 1;

            // Parse the stream body. ISO 32000-2:2020 §7.6.3 says ObjStm
            // shall NOT be individually encrypted, so skip decryption here
            // — mirrors the default branch in `load_compressed_object`.
            let objects_map = match parse_object_stream_with_decryption(&stream_obj, None, 0, 0) {
                Ok(m) => m,
                Err(e) => {
                    log::debug!(
                        "Skipping ObjStm {} during recovery sweep (parse failed: {})",
                        stream_obj_num,
                        e
                    );
                    continue;
                }
            };

            let mut cache = self.object_cache.lock_or_recover();
            for (obj_num, object) in objects_map {
                let cache_ref = ObjectRef::new(obj_num, 0);
                // Only overwrite entries we'd otherwise have resolved to
                // Null (the free-entry short-circuit caches Null). Never
                // clobber a real object loaded through the normal path.
                match cache.get(&cache_ref) {
                    Some(Object::Null) | None => {
                        cache.insert(cache_ref, object);
                        recovered += 1;
                    }
                    _ => {}
                }
            }
        }

        log::debug!(
            "Object-stream recovery sweep: {} candidate positions, {} ObjStms, {} objects cached",
            candidates.len(),
            objstms_found,
            recovered
        );

        *self.objstm_recovery_done.lock_or_recover() = true;
    }

    /// Load an object by its reference.
    ///
    /// This function:
    /// 1. Checks the object cache first
    /// 2. If not cached, looks up the byte offset in the xref table
    /// 3. Seeks to that offset and parses the object
    /// 4. Caches the result for future access
    /// 5. If object not in xref but is critical, scans file for it
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The object reference is not in the xref table and file scan fails
    /// - The object is not in use (free object)
    /// - Seeking to the object offset fails
    /// - Parsing the object fails
    /// - A circular reference is detected
    /// - The recursion depth limit is exceeded
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use pdf_oxide::document::PdfDocument;
    /// # use pdf_oxide::object::ObjectRef;
    /// # let mut doc = PdfDocument::open("sample.pdf")?;
    /// let obj_ref = ObjectRef::new(1, 0);
    /// let obj = doc.load_object(obj_ref)?;
    /// # Ok::<(), pdf_oxide::error::Error>(())
    /// ```
    pub fn load_object(&self, obj_ref: ObjectRef) -> Result<Object> {
        log::debug!("Loading object {} gen {}", obj_ref.id, obj_ref.gen);

        // Check recursion depth (per-thread counter; no lock needed)
        {
            let depth = RECURSION_DEPTH.with(|d| *d.borrow());
            if depth >= MAX_RECURSION_DEPTH {
                log::error!(
                    "Recursion depth limit exceeded ({}) while loading object {} gen {}",
                    MAX_RECURSION_DEPTH,
                    obj_ref.id,
                    obj_ref.gen
                );
                return Err(Error::RecursionLimitExceeded(MAX_RECURSION_DEPTH));
            }
        }

        // Check for circular references (per-thread stack; concurrent threads
        // resolving the same object do NOT appear as a false cycle)
        if RESOLVING_STACK.with(|s| s.borrow().contains(&obj_ref)) {
            log::error!(
                "Circular reference detected for object {} gen {} (depth: {})",
                obj_ref.id,
                obj_ref.gen,
                RECURSION_DEPTH.with(|d| *d.borrow())
            );
            return Err(Error::CircularReference(obj_ref));
        }

        // Check cache first (warm path: fully parallel, no serialization).
        let cached_opt = self.object_cache.lock_or_recover().get(&obj_ref).cloned();
        if let Some(cached) = cached_opt {
            return Ok(cached);
        }

        // Cold path (#507): serialize uncached loads across threads so a
        // single logical load's many `reader` lock scopes are not
        // interleaved by another thread's load on the shared `BufReader`.
        // Acquire ONLY at the top-level entry (recursion depth 0); a
        // recursive call from this same thread (nested-ref resolution)
        // already holds the guard, so re-acquiring would self-deadlock —
        // skip it. Held for the remainder of this top-level resolution.
        let _load_guard = if RECURSION_DEPTH.with(|d| *d.borrow()) == 0 {
            let guard = self.load_lock.lock_or_recover();
            // Double-checked: another thread may have loaded and cached
            // this object while we were blocked on the guard.
            if let Some(cached) = self.object_cache.lock_or_recover().get(&obj_ref).cloned() {
                return Ok(cached);
            }
            Some(guard)
        } else {
            None
        };

        // Look up in xref table
        let entry = match self.xref.get(obj_ref.id) {
            Some(entry) => entry,
            None => {
                // Object not in xref table - try scanning the file as fallback
                // This handles PDFs with incomplete/corrupted xref tables
                let available: Vec<u32> = self.xref.entries.keys().copied().take(20).collect();
                log::warn!(
                    "Object {} not in xref table. Total entries: {}. First 20 objects: {:?}",
                    obj_ref.id,
                    self.xref.len(),
                    available
                );

                // Try to scan the file for this object
                match self.scan_for_object(obj_ref) {
                    Ok(offset) => {
                        // Found it! Load directly from this offset
                        log::info!(
                            "Successfully found object {} via file scan at offset {}",
                            obj_ref.id,
                            offset
                        );

                        // Mark as being resolved (per-thread cycle detection)
                        RESOLVING_STACK.with(|s| {
                            s.borrow_mut().insert(obj_ref);
                        });
                        RECURSION_DEPTH.with(|d| *d.borrow_mut() += 1);

                        // Load the object
                        let result = self.load_uncompressed_object(obj_ref, offset);

                        RECURSION_DEPTH.with(|d| *d.borrow_mut() -= 1);
                        RESOLVING_STACK.with(|s| {
                            s.borrow_mut().remove(&obj_ref);
                        });

                        return result;
                    }
                    Err(_) => {
                        // PDF Spec §7.3.10: missing object reference "shall be treated as null"
                        log::warn!("Object {} gen {} not found (xref + file scan failed), treating as Null per §7.3.10", obj_ref.id, obj_ref.gen);
                        self.object_cache
                            .lock_or_recover()
                            .insert(obj_ref, Object::Null);
                        return Ok(Object::Null);
                    }
                }
            }
        };

        log::debug!(
            "  → Found in xref: type={:?}, offset={}, gen={}, in_use={}",
            entry.entry_type,
            entry.offset,
            entry.generation,
            entry.in_use
        );

        // Check if object is in use
        if !entry.in_use {
            log::debug!(
                "Object {} is marked as free (not in use). This may be due to a corrupted xref table.",
                obj_ref.id
            );

            // xref flags the object free, but this may be xref corruption
            // rather than an actual deletion. Run two recovery paths before
            // falling back to §7.3.10's null. The branches below apply
            // uniformly for all object ids (critical low-numbered catalog
            // objects and page objects in the thousands); previously low
            // ids took a separate "fall through to loading logic" path
            // that silently hit the Free arm of the entry_type match
            // still ended up Null.
            //
            // Recovery path 1 — standalone `N G obj` marker in the file
            // body. `scan_for_object` builds a whole-file offset map once
            // per document and caches it, so the amortised cost is a
            // single O(filesize) pass no matter how many free-marked
            // objects we probe.
            if let Ok(scanned_offset) = self.scan_for_object(obj_ref) {
                log::debug!(
                    "Object {} marked free in xref but found in file scan at offset {}; recovering",
                    obj_ref.id,
                    scanned_offset
                );
                RESOLVING_STACK.with(|s| {
                    s.borrow_mut().insert(obj_ref);
                });
                RECURSION_DEPTH.with(|d| *d.borrow_mut() += 1);
                let result = self.load_uncompressed_object(obj_ref, scanned_offset);
                RECURSION_DEPTH.with(|d| *d.borrow_mut() -= 1);
                RESOLVING_STACK.with(|s| {
                    s.borrow_mut().remove(&obj_ref);
                });
                return result;
            }

            // Recovery path 2 — the object may be compressed inside a
            // `/Type /ObjStm`. Real-world producers have been seen to
            // mis-flag every compressed object's xref slot as free, so
            // sweep the object streams once and recheck the cache.
            self.recover_from_object_streams();
            if let Some(obj) = self.object_cache.lock_or_recover().get(&obj_ref).cloned() {
                if !matches!(obj, Object::Null) {
                    log::debug!("Object {} recovered from object-stream sweep", obj_ref.id);
                    return Ok(obj);
                }
            }

            // PDF Spec §7.3.10: free object treated as null
            log::warn!(
                "Free object {} gen {}, treating as Null per §7.3.10",
                obj_ref.id,
                obj_ref.gen
            );
            self.object_cache
                .lock_or_recover()
                .insert(obj_ref, Object::Null);
            return Ok(Object::Null);
        }

        // Mark as being resolved (per-thread cycle detection)
        RESOLVING_STACK.with(|s| {
            s.borrow_mut().insert(obj_ref);
        });
        RECURSION_DEPTH.with(|d| *d.borrow_mut() += 1);

        // Handle different entry types
        use crate::xref::XRefEntryType;
        let entry_type = entry.entry_type;
        let entry_offset = entry.offset;
        let entry_gen = entry.generation;
        let result = match entry_type {
            XRefEntryType::Compressed => {
                // Type 2 entry: object is in an object stream
                // entry.offset = stream object number
                // entry.generation = index within stream
                log::debug!(
                    "  → Compressed object in stream {}, index {}",
                    entry_offset,
                    entry_gen
                );
                self.load_compressed_object(obj_ref, entry_offset as u32, entry_gen)
            }
            XRefEntryType::Uncompressed => {
                // Type 1 entry: traditional uncompressed object
                log::debug!("  → Uncompressed object at offset {}", entry_offset);
                self.load_uncompressed_object(obj_ref, entry_offset)
            }
            XRefEntryType::Free => {
                // Free object - shouldn't happen since we check in_use above
                // PDF Spec §7.3.10: treat as null
                log::warn!(
                    "Object {} has type Free despite in_use=true, treating as Null",
                    obj_ref.id
                );
                self.object_cache
                    .lock_or_recover()
                    .insert(obj_ref, Object::Null);
                Ok(Object::Null)
            }
        };

        RECURSION_DEPTH.with(|d| *d.borrow_mut() -= 1);
        RESOLVING_STACK.with(|s| {
            s.borrow_mut().remove(&obj_ref);
        });

        result
    }

    /// Resolve references within an object recursively.
    ///
    /// This utility method resolves indirect references within an object,
    /// handling nested dictionaries and arrays up to a specified depth.
    /// Useful for processing complex PDF structures where properties
    /// may be stored as indirect references.
    ///
    /// # Arguments
    ///
    /// * `obj` - The object to resolve references within
    /// * `max_depth` - Maximum recursion depth to prevent infinite loops
    ///
    /// # Returns
    ///
    /// The object with all references resolved up to max_depth levels.
    /// If a reference cannot be resolved, it is left as-is.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use pdf_oxide::document::PdfDocument;
    /// # let mut doc = PdfDocument::open("sample.pdf")?;
    /// # let obj = doc.catalog()?;
    /// // Resolve all references in a dictionary up to 3 levels deep
    /// let resolved = doc.resolve_references(&obj, 3)?;
    /// # Ok::<(), pdf_oxide::error::Error>(())
    /// ```
    pub fn resolve_references(&self, obj: &Object, max_depth: usize) -> Result<Object> {
        if max_depth == 0 {
            return Ok(obj.clone());
        }

        match obj {
            Object::Reference(obj_ref) => {
                // Resolve the reference
                match self.load_object(*obj_ref) {
                    Ok(resolved) => {
                        // Recursively resolve within the resolved object
                        self.resolve_references(&resolved, max_depth - 1)
                    }
                    Err(e) => {
                        log::warn!("Failed to resolve reference {:?}: {}", obj_ref, e);
                        Ok(obj.clone()) // Return the unresolved reference
                    }
                }
            }

            Object::Dictionary(dict) => {
                // Resolve references within each value
                let mut resolved_dict = std::collections::HashMap::new();
                for (key, value) in dict.iter() {
                    let resolved_value = self.resolve_references(value, max_depth - 1)?;
                    resolved_dict.insert(key.clone(), resolved_value);
                }
                Ok(Object::Dictionary(resolved_dict))
            }

            Object::Array(arr) => {
                // Resolve references within each element
                let resolved_arr: Result<Vec<Object>> = arr
                    .iter()
                    .map(|item| self.resolve_references(item, max_depth - 1))
                    .collect();
                Ok(Object::Array(resolved_arr?))
            }

            // For all other types, just return a clone
            _ => Ok(obj.clone()),
        }
    }

    /// Resolve a single-level indirect reference (PDF spec §7.3.10).
    ///
    /// If `obj` is `Object::Reference(...)`, loads and returns the target object.
    /// For any other object type, returns a clone unchanged. This is the
    /// canonical way to handle "any value may be a direct or indirect reference"
    /// throughout the parser.
    pub(super) fn resolve_obj_ref(&self, obj: &Object) -> Object {
        if let Some(obj_ref) = obj.as_reference() {
            match self.load_object(obj_ref) {
                Ok(resolved) => resolved,
                Err(e) => {
                    log::warn!("Failed to resolve indirect reference {:?}: {}", obj_ref, e);
                    obj.clone()
                }
            }
        } else {
            obj.clone()
        }
    }

    /// Peek at an XObject's /Subtype without loading the full object.
    /// Returns true if the XObject is a Form XObject, false if Image or unknown.
    /// For compressed objects or on any error, returns true (conservative — will load fully).
    pub fn is_form_xobject(&self, obj_ref: ObjectRef) -> bool {
        // Check negative cache first (known non-Form XObjects)
        {
            if self
                .image_xobject_cache
                .lock_or_recover()
                .contains(&obj_ref)
            {
                return false;
            }
        }

        // If already in object cache, check directly
        let cached_opt = self.object_cache.lock_or_recover().get(&obj_ref).cloned();
        if let Some(cached) = cached_opt {
            let is_form = cached
                .as_dict()
                .and_then(|d| d.get("Subtype"))
                .and_then(|s| s.as_name())
                == Some("Form");
            if !is_form {
                self.image_xobject_cache.lock_or_recover().insert(obj_ref);
            }
            return is_form;
        }

        // Look up in xref table
        let entry = match self.xref.get(obj_ref.id) {
            Some(e) => e,
            None => return true, // conservative fallback
        };

        // Only peek uncompressed objects — compressed ones require full load
        use crate::xref::XRefEntryType;
        if entry.entry_type != XRefEntryType::Uncompressed || !entry.in_use {
            return true; // conservative fallback
        }

        // Seek + read under a SINGLE lock guard. Splitting the seek
        // the read across two `self.reader.lock_or_recover()` acquisitions
        // is the #398 Race A split-lock bug (same one already fixed in
        // `load_uncompressed_object_impl`): a concurrent thread can
        // re-seek the shared reader between our seek() and read(), so we
        // read a garbage buffer for a different object. That surfaced as
        // a spurious `[1000] invalid PDF structure or content stream`
        // ParseError under concurrent `render_page_fit`.
        let offset = entry.offset;
        let mut buf = [0u8; 1024];
        let n = {
            let mut reader = self.reader.lock_or_recover();
            if reader.seek(SeekFrom::Start(offset)).is_err() {
                return true;
            }
            // Read enough bytes for the object header + dictionary (<1KB)
            match reader.read(&mut buf) {
                Ok(n) => n,
                Err(_) => return true,
            }
        };
        let data = &buf[..n];

        // Search for /Subtype in the buffer
        // Look for "/Subtype" followed by a name like "/Form" or "/Image"
        if let Some(pos) = data.windows(8).position(|w| w == b"/Subtype") {
            let after = &data[pos + 8..];
            // Skip whitespace
            let trimmed = after
                .iter()
                .position(|&b| b != b' ' && b != b'\t' && b != b'\r' && b != b'\n');
            if let Some(start) = trimmed {
                let name_data = &after[start..];
                if name_data.starts_with(b"/Form") {
                    return true;
                }
                // Image, PS, or anything else — not a Form
                self.image_xobject_cache.lock_or_recover().insert(obj_ref);
                return false;
            }
        }

        // /Subtype not found in first 1KB — conservative fallback
        true
    }

    /// Load an uncompressed object (Type 1 xref entry).
    pub(super) fn load_uncompressed_object(
        &self,
        obj_ref: ObjectRef,
        offset: u64,
    ) -> Result<Object> {
        self.load_uncompressed_object_impl(obj_ref, offset, false)
    }
}
