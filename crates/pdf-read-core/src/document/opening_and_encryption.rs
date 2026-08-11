use super::parsing::*;
use super::preflight::*;
use super::*;

impl PdfDocument {
    #[cfg(test)]
    pub(crate) fn from_bytes(data: Vec<u8>) -> Result<Self> {
        let reader = PdfReader::Memory(BufReader::new(Cursor::new(data)));
        Self::open_from_reader(
            reader,
            DEFAULT_OBJECT_CACHE_MAX_BYTES,
            DEFAULT_XOBJECT_CACHE_MAX_ENTRIES,
        )
    }

    pub(crate) fn from_file_with_limits(
        mut file: File,
        object_cache_bytes: usize,
        xobject_cache_entries: usize,
    ) -> Result<Self> {
        file.seek(SeekFrom::Start(0))?;
        Self::open_from_reader(
            PdfReader::File(BufReader::new(file)),
            object_cache_bytes,
            xobject_cache_entries,
        )
    }

    pub(super) fn open_from_reader(
        mut reader: PdfReader,
        object_cache_bytes: usize,
        xobject_cache_entries: usize,
    ) -> Result<Self> {
        // Parse header with lenient mode by default (handle PDFs with binary prefixes)
        let (major, minor, header_offset) = parse_header(&mut reader, true)?;
        let version = (major, minor);

        // Whether the xref table below came from a full-file reconstruction
        // scan (vs. a parsed xref). Used to pre-seed the object-scan cache so
        // a later miss doesn't rescan the whole file a second time (#572).
        let mut xref_reconstructed = false;
        // SYNTHETIC objects a recovery invented (a rebuilt Catalog / page-tree
        // root for a truncated file). They have no byte offset, so they are
        // seeded into the object cache after the document is built. Empty in the
        // ordinary case.
        let mut synthetic_objects: Vec<(ObjectRef, Object)> = Vec::new();

        // Try to parse xref table normally
        let (mut xref, trailer) = match Self::try_open_regular(&mut reader) {
            Ok((xref, trailer)) => {
                // Success with regular parsing
                // However, if the xref is suspiciously small (< 5 entries), it's likely corrupted
                // Try reconstruction to get a complete table
                if xref.is_empty() {
                    log::warn!(
                        "Regular xref parsing succeeded but table is empty, attempting reconstruction"
                    );
                    xref_reconstructed = true;
                    let (x, t, syn) = Self::try_reconstruct_xref(&mut reader)?;
                    synthetic_objects = syn;
                    (x, t)
                } else {
                    // A valid xref can have any number of entries (§7.5.4).
                    // Small xrefs (e.g. portfolio PDFs with 3-4 objects) are perfectly
                    // normal — don't trigger expensive full-file reconstruction for them.
                    (xref, trailer)
                }
            }
            Err(e) => {
                log::warn!(
                    "Regular xref parsing failed: {}, attempting reconstruction",
                    e
                );

                // Fall back to xref reconstruction
                match Self::try_reconstruct_xref(&mut reader) {
                    Ok((reconstructed_xref, reconstructed_trailer, syn)) => {
                        log::info!("Successfully reconstructed xref table");
                        xref_reconstructed = true;
                        synthetic_objects = syn;
                        (reconstructed_xref, reconstructed_trailer)
                    }
                    Err(recon_err) => {
                        log::error!("XRef reconstruction also failed: {}", recon_err);
                        // The original parse error is the more useful diagnosis for a
                        // damaged file, but a budget refusal or cancellation is not a
                        // diagnosis of the file at all — reporting it as `InvalidXref`
                        // would blame the input for our own ceiling.
                        if matches!(recon_err, Error::ResourceLimit { .. } | Error::Cancelled) {
                            return Err(recon_err);
                        }
                        return Err(e); // Return original error
                    }
                }
            }
        };

        // If PDF header is not at byte 0 (garbage-prepended), xref offsets may need adjustment.
        // The xref offsets are relative to the original PDF start, but file positions are
        // shifted by header_offset bytes.
        if header_offset > 0 {
            // Probe an object to decide whether xref offsets are off by
            // header_offset. Prefer /Root (common case), but the probe MUST
            // be seek-validatable: `validate_object_at_offset` returns true
            // for *compressed* entries without seeking, so a /Root that
            // lives in an object stream would falsely report "no shift
            // needed" and leave every uncompressed offset wrong. Use /Root
            // only when its entry is in-use + uncompressed; otherwise (no
            // /Root — issue #509 — or a compressed /Root) fall back to the
            // first in-use uncompressed object.
            let probe = get_root_ref_from_trailer(&trailer)
                .filter(|r| {
                    xref.get(r.id).is_some_and(|e| {
                        e.in_use && e.entry_type == crate::xref::XRefEntryType::Uncompressed
                    })
                })
                .or_else(|| first_in_use_uncompressed(&xref));
            if let Some(probe_ref) = probe {
                if !validate_object_at_offset(&mut reader, &xref, probe_ref) {
                    log::info!(
                        "Probe object {} not loadable at xref offset, adjusting all offsets by header_offset={}",
                        probe_ref.id, header_offset
                    );
                    xref.shift_offsets(header_offset);
                }
            }
        }

        // Validate the /Root catalog is actually loadable. If not, the xref data is
        // corrupt despite parsing successfully — fall back to reconstruction.
        let (xref, trailer) = if !validate_root_loadable(&mut reader, &xref, &trailer) {
            log::warn!(
                "Root object not loadable after xref parse, falling back to xref reconstruction"
            );
            match Self::try_reconstruct_xref(&mut reader) {
                Ok((x, t, syn)) => {
                    xref_reconstructed = true;
                    synthetic_objects = syn;
                    (x, t)
                }
                Err(_) => (xref, trailer), // Use original if reconstruction also fails
            }
        } else {
            (xref, trailer)
        };

        // #572: a reconstruction scan already located every uncompressed
        // "N G obj" in the file, so a later scan_for_object full-file rescan
        // (on the first object miss) would find nothing new — it just repeats
        // the work, the ~25 s "first extract_text" cost on corrupt-xref
        // polyglots. Pre-seed the scan-offset cache from the reconstructed
        // table so that first miss is O(1). Only do this when reconstructed:
        // a normal (parsed) xref may be legitimately partial, and there the
        // full scan is the intended recovery path.
        let prepopulated_scan: Option<HashMap<u32, u64>> = if xref_reconstructed {
            Some(
                xref.all_object_numbers()
                    .filter_map(|id| {
                        xref.get(id).and_then(|e| {
                            (e.in_use && e.entry_type == crate::xref::XRefEntryType::Uncompressed)
                                .then_some((id, e.offset))
                        })
                    })
                    .collect(),
            )
        } else {
            None
        };

        // Note: Encryption initialization was originally lazy, but decode_stream_with_encryption
        // only has &self access which prevents initialization.
        // We now initialize eagerly to ensure the handler is ready when needed.
        let document = Self {
            reader: Mutex::new(reader),
            load_lock: Mutex::new(()),
            version,
            xref,
            trailer,
            object_cache: Mutex::new(BoundedObjectCache::new(object_cache_bytes)),
            encryption_handler: Mutex::new(None),
            encrypt_dict_ref: Mutex::new(None),
            options: ParserOptions::default(),
            header_offset,
            font_cache: Mutex::new(BoundedEntryCache::new(512)),
            font_set_cache: Mutex::new(BoundedEntryCache::new(256)),
            font_fingerprint_cache: Mutex::new(BoundedEntryCache::new(256)),
            font_name_set_cache: Mutex::new(BoundedEntryCache::new(256)),
            font_identity_cache: Mutex::new(BoundedEntryCache::new(512)),
            font_id_hash_cache: Mutex::new(HashMap::new()),
            structure_tree_cache: Mutex::new(None),
            structure_content_cache: Mutex::new(None),
            actualtext_index_cache: Mutex::new(None),
            mc_actualtext_mcids: Mutex::new(HashMap::new()),
            table_elements_cache: Mutex::new(None),
            page_cache: Mutex::new(HashMap::new()),
            page_cache_populated: AtomicBool::new(false),
            scanned_object_offsets: Mutex::new(prepopulated_scan),
            objstm_recovery_done: Mutex::new(false),
            image_xobject_cache: Mutex::new(HashSet::new()),
            xobject_text_free_cache: Mutex::new(HashSet::new()),
            xobject_stream_cache: Mutex::new(HashMap::new()),
            xobject_stream_cache_bytes: AtomicUsize::new(0),
            xobject_spans_cache: Mutex::new(BoundedEntryCache::new(xobject_cache_entries)),
            form_xobject_images_cache: Mutex::new(BoundedEntryCache::new(xobject_cache_entries)),
            page_content_cache: Mutex::new(BoundedEntryCache::new(64)),
            page_spans_cache: Mutex::new(BoundedEntryCache::new(8)),
            page_chars_cache: Mutex::new(BoundedEntryCache::new(8)),
            running_artifact_signatures: Mutex::new(None),
            article_threads_cache: Mutex::new(None),
            output_intent_cmyk_profile_cache: Mutex::new(None),
            accumulated_warnings: Mutex::new(Vec::new()),
            warning_sink: crate::extractors::warnings::WarningSink::new(),
        };

        // Seed any SYNTHETIC recovery objects (a Catalog / page-tree root rebuilt
        // for a truncated file) into the object cache. They have no byte offset,
        // so `load_object` - which checks the cache before the xref - is the only
        // way to reach them. Done before encryption init so the /Root resolves.
        if !synthetic_objects.is_empty() {
            let mut cache = document.object_cache.lock_or_recover();
            for (obj_ref, obj) in synthetic_objects {
                cache.insert(obj_ref, obj);
            }
        }

        // Initialize encryption immediately
        if let Err(e) = document.ensure_encryption_initialized() {
            log::error!("Failed to initialize encryption: {}", e);
            // We continue anyway, as it might just be an unsupported security handler
            // and maybe we can still read parts of the file (or fail later)
        }

        Ok(document)
    }

    /// Try to open the PDF using regular xref parsing.
    pub(super) fn try_open_regular<R: Read + Seek>(
        reader: &mut R,
    ) -> Result<(CrossRefTable, Object)> {
        // Find xref table offset
        let xref_offset = find_xref_offset(reader)?;

        // Parse xref table
        let xref = parse_xref(reader, xref_offset)?;

        // Get trailer dictionary
        let trailer = if let Some(trailer_dict) = xref.trailer() {
            // XRef stream: trailer is already in the xref table
            Object::Dictionary(trailer_dict.clone())
        } else {
            // Traditional xref: parse trailer separately
            reader.seek(SeekFrom::Start(xref_offset))?;
            parse_trailer(reader)?
        };

        Ok((xref, trailer))
    }

    /// Try to reconstruct the xref table by scanning the file. The third tuple
    /// element is any SYNTHETIC objects (a rebuilt Catalog / page-tree root for a
    /// truncated file) the caller must seed into the object cache - empty in the
    /// ordinary case.
    pub(super) fn try_reconstruct_xref<R: Read + Seek>(
        reader: &mut R,
    ) -> Result<(CrossRefTable, Object, Vec<(ObjectRef, Object)>)> {
        crate::xref_reconstruction::reconstruct_xref(reader)
    }

    /// Initialize encryption handler lazily if PDF is encrypted.
    ///
    /// PDF Spec: Section 7.6.1 - Encryption dictionary in trailer
    ///
    /// This checks for the /Encrypt entry in the trailer, loads it if it's a
    /// reference, and creates an encryption handler. It automatically attempts
    /// to authenticate with an empty password (common for PDFs with default encryption).
    ///
    /// This is called lazily the first time we need to decrypt something, after
    /// the document is fully constructed and can load objects.
    pub(super) fn ensure_encryption_initialized(&self) -> Result<()> {
        // Already initialized?
        if self.encryption_handler.lock_or_recover().is_some() {
            return Ok(());
        }

        // Clone what we need from trailer to avoid borrow conflicts
        let (encrypt_ref, file_id) = {
            let trailer_dict = match self.trailer.as_dict() {
                Some(d) => d,
                None => return Ok(()), // No trailer dict, no encryption
            };

            // Check for /Encrypt entry
            let encrypt_entry = match trailer_dict.get("Encrypt") {
                Some(obj) => obj,
                None => {
                    log::debug!("PDF is not encrypted (no /Encrypt entry)");
                    return Ok(());
                }
            };

            // Clone the encrypt entry (we'll load it outside this block)
            let encrypt_ref = encrypt_entry.clone();

            // Get file ID (required for encryption key derivation)
            let file_id = match trailer_dict.get("ID") {
                Some(Object::Array(arr)) => {
                    if let Some(first_id) = arr.first() {
                        if let Some(id_bytes) = first_id.as_string() {
                            id_bytes.to_vec()
                        } else {
                            log::warn!(
                                "Invalid /ID array entry (not a string), using empty file ID"
                            );
                            vec![]
                        }
                    } else {
                        log::warn!("Empty /ID array, using empty file ID");
                        vec![]
                    }
                }
                _ => {
                    log::warn!("Missing or invalid /ID entry in trailer, using empty file ID");
                    vec![]
                }
            };

            (encrypt_ref, file_id)
        }; // End of borrow scope

        // Now load the encrypt object (dereference if needed)
        let encrypt_obj = match encrypt_ref {
            Object::Dictionary(_) => encrypt_ref,
            Object::Reference(obj_ref) => {
                log::debug!(
                    "Loading /Encrypt object reference {} {}",
                    obj_ref.id,
                    obj_ref.gen
                );
                // Remember which object holds the /Encrypt dict so its own
                // strings are skipped during per-object string decryption.
                *self.encrypt_dict_ref.lock_or_recover() = Some(obj_ref);
                self.load_object(obj_ref)?
            }
            _ => {
                return Err(Error::InvalidPdf(format!(
                    "Invalid /Encrypt entry type: {}",
                    encrypt_ref.type_name()
                )));
            }
        };

        // Resolve any indirect references within the encrypt dictionary.
        // Some PDFs store /O, /U, /V, /R, /P as indirect references (e.g., `7 0 R`).
        let encrypt_obj = if let Some(dict) = encrypt_obj.as_dict() {
            let mut resolved_dict = dict.clone();
            for value in resolved_dict.values_mut() {
                if let Object::Reference(obj_ref) = value {
                    match self.load_object(*obj_ref) {
                        Ok(resolved) => *value = resolved,
                        Err(e) => {
                            log::warn!("Failed to resolve indirect ref in /Encrypt dict: {}", e);
                        }
                    }
                }
            }
            Object::Dictionary(resolved_dict)
        } else {
            encrypt_obj
        };

        // Create encryption handler with the file_id we extracted above
        let mut handler = EncryptionHandler::new(&encrypt_obj, file_id)?;

        // Try to authenticate with empty password (common default)
        match handler.authenticate(b"") {
            Ok(true) => {
                log::info!("Successfully authenticated with empty password");
            }
            Ok(false) => {
                log::warn!("PDF is encrypted and requires a password");
                self.push_warning("PDF is encrypted and requires a password".to_string());
            }
            Err(e) => {
                log::error!("Failed to initialize encryption: {}", e);
                return Err(e);
            }
        }

        *self.encryption_handler.lock_or_recover() = Some(handler);
        Ok(())
    }

    /// Decode stream data with encryption support.
    ///
    /// This is a helper method that decodes stream data using the PDF's encryption handler
    /// if the document is encrypted. It automatically handles object-specific key derivation.
    ///
    /// # Arguments
    ///
    /// * `stream_obj` - The stream object to decode
    /// * `obj_ref` - The object reference (for encryption key derivation)
    ///
    /// # Returns
    ///
    /// The decoded (and decrypted if needed) stream data.
    ///
    /// # PDF Spec Reference
    ///
    /// ISO 32000-1:2008, Section 7.6.2 - Streams must be decrypted BEFORE applying filters.
    pub(crate) fn decode_stream_with_encryption(
        &self,
        stream_obj: &Object,
        obj_ref: ObjectRef,
    ) -> Result<Vec<u8>> {
        if matches!(stream_obj, Object::Null) {
            return Ok(Vec::new());
        }

        // Per ISO 32000-2:2020 Section 7.6.3, object streams (/Type /ObjStm)
        // and cross-reference streams (/Type /XRef) shall NOT be encrypted.
        // Skip decryption for these stream types to avoid AES block-size errors
        // on data that was never encrypted in the first place.
        let is_unencrypted_stream_type = if let Object::Stream { dict, .. } = stream_obj {
            dict.get("Type")
                .and_then(|t| t.as_name())
                .map(|name| name == "ObjStm" || name == "XRef")
                .unwrap_or(false)
        } else {
            false
        };

        let handler_ref = self.encryption_handler.lock_or_recover();
        if let Some(handler) = handler_ref.as_ref() {
            if is_unencrypted_stream_type {
                // These stream types are never encrypted per spec
                drop(handler_ref);
                return stream_obj.decode_stream_data();
            }
            // Create decryption closure for this specific object
            let decrypt_fn = |data: &[u8]| -> Result<Vec<u8>> {
                handler.decrypt_stream(data, obj_ref.id, obj_ref.gen as u32)
            };
            stream_obj.decode_stream_data_with_decryption(
                Some(&decrypt_fn),
                obj_ref.id,
                obj_ref.gen as u32,
            )
        } else {
            drop(handler_ref);
            // No encryption, use regular decoding
            stream_obj.decode_stream_data()
        }
    }

    /// Check if the PDF is encrypted.
    ///
    /// Returns `true` if the PDF has an `/Encrypt` entry in its trailer,
    /// regardless of whether it has been authenticated.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use pdf_oxide::document::PdfDocument;
    /// # let mut doc = PdfDocument::open("sample.pdf")?;
    /// if doc.is_encrypted() {
    ///     println!("PDF is encrypted");
    /// }
    /// # Ok::<(), pdf_oxide::error::Error>(())
    /// ```
    pub fn is_encrypted(&self) -> bool {
        // Check if encryption handler is already initialized
        if self.encryption_handler.lock_or_recover().is_some() {
            return true;
        }
        // Check trailer for /Encrypt entry without initializing
        self.trailer
            .as_dict()
            .and_then(|d| d.get("Encrypt"))
            .is_some()
    }

    /// Whether content extraction is permitted right now — `true` if the
    /// PDF is unencrypted, or encrypted and successfully authenticated.
    ///
    /// Cheap, side-effect-free preflight for the auto-extraction
    /// classifier (#517): lets it emit
    /// [`ReasonCode::EncryptedNoExtractPermission`](crate::extractors::auto::ReasonCode)
    /// gracefully instead of attempting extraction and erroring.
    #[must_use]
    pub fn is_authenticated(&self) -> bool {
        // Fail closed: if encryption init errors (malformed / unsupported
        // `/Encrypt`), the document IS encrypted but we cannot have
        // authenticated it — a security preflight must report `false`
        // here, not `true` (PR #519 review). Only when init succeeds
        // (incl. the trivial unencrypted case) do we trust the guard.
        if self.ensure_encryption_initialized().is_err() {
            return false;
        }
        !self.is_encrypted_and_unauthenticated()
    }

    /// Check if the PDF is encrypted but has NOT been successfully authenticated.
    ///
    /// This returns `true` when the document requires a password that has not
    /// yet been provided. Extraction methods use this to return a clear error
    /// instead of silently producing empty output.
    pub(super) fn is_encrypted_and_unauthenticated(&self) -> bool {
        if let Some(handler) = self.encryption_handler.lock_or_recover().as_ref() {
            !handler.is_authenticated()
        } else {
            // Handler not yet initialized — check if /Encrypt exists
            // If it does, we don't know auth state yet, so return false
            // (ensure_encryption_initialized will handle it)
            false
        }
    }

    /// Guard that returns `Err(Error::EncryptedPdf)` if the PDF is encrypted
    /// and not authenticated. Call this at the top of extraction methods.
    pub(super) fn require_authenticated(&self) -> Result<()> {
        // Make sure encryption is initialized first
        self.ensure_encryption_initialized()?;
        if self.is_encrypted_and_unauthenticated() {
            return Err(Error::EncryptedPdf);
        }
        Ok(())
    }

    /// True once the empty user password has been tried and the document is
    /// still locked. Text extraction degrades to empty output in this case
    /// (matching pdftotext/PyMuPDF) rather than erroring; `page_count` and
    /// write paths keep using [`Self::require_authenticated`].
    pub(super) fn is_encrypted_unreadable(&self) -> bool {
        let _ = self.ensure_encryption_initialized();
        self.is_encrypted_and_unauthenticated()
    }

    /// Get the PDF version.
    ///
    /// Returns a tuple (major, minor) representing the PDF version.
    /// For example, PDF 1.7 returns (1, 7).
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use pdf_oxide::document::PdfDocument;
    /// # let mut doc = PdfDocument::open("sample.pdf")?;
    /// let (major, minor) = doc.version();
    /// println!("PDF version: {}.{}", major, minor);
    /// # Ok::<(), pdf_oxide::error::Error>(())
    /// ```
    pub fn version(&self) -> (u8, u8) {
        self.version
    }

    /// Get a reference to the trailer dictionary.
    ///
    /// The trailer dictionary contains important document metadata including:
    /// - /Root: Reference to the catalog dictionary
    /// - /Info: Reference to the document info dictionary (optional)
    /// - /Size: Number of entries in the cross-reference table
    /// - /Encrypt: Encryption dictionary (if encrypted)
    /// - /ID: File identifier array
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use pdf_oxide::document::PdfDocument;
    /// # let mut doc = PdfDocument::open("sample.pdf")?;
    /// let trailer = doc.trailer();
    /// if let Some(dict) = trailer.as_dict() {
    ///     if let Some(info_ref) = dict.get("Info") {
    ///         println!("Document has an Info dictionary");
    ///     }
    /// }
    /// # Ok::<(), pdf_oxide::error::Error>(())
    /// ```
    pub fn trailer(&self) -> &Object {
        &self.trailer
    }

    /// Return every object ID known to this document.
    ///
    /// Unions the cross-reference table with any object IDs that were
    /// recovered from compressed object streams (which may not have an
    /// explicit xref entry). The result is sorted and deduplicated so
    /// callers can iterate once and write each object exactly once.
    ///
    /// Used by `DocumentEditor::write_full_to_writer` to sweep any
    /// objects that were not reached during the shallow page-tree
    /// traversal (e.g. embedded font sub-objects such as
    /// `DescendantFonts`, `FontFile2`, `ToUnicode`, `FontDescriptor`).
    pub fn all_object_ids(&self) -> Vec<u32> {
        let mut ids: Vec<u32> = self.xref.all_object_numbers().collect();
        for r in self.object_cache.lock_or_recover().keys() {
            ids.push(r.id);
        }
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    /// Return references to every leaf page, in document order, with a single
    /// page-tree traversal.
    ///
    /// Replaces the O(n²) pattern of calling [`get_page_ref`] in a 0..n loop:
    /// each `get_page_ref(i)` walks the tree from the root and stops at the
    /// i-th leaf, so collecting all n refs walks 1+2+...+n nodes.
    ///
    /// Optimised for the common flat-tree case: when a `Pages` node's
    /// `Count` matches `Kids.len()`, every kid is a leaf and we can take
    /// the references straight from the array without loading each leaf.
    /// Only when the tree is multi-level do we recurse and load child nodes.
    pub(crate) fn all_page_refs(&self) -> Result<Vec<ObjectRef>> {
        let catalog = self.catalog()?;
        let catalog_dict = catalog.as_dict().ok_or_else(|| Error::InvalidObjectType {
            expected: "Dictionary".to_string(),
            found: catalog.type_name().to_string(),
        })?;
        let pages_ref = catalog_dict
            .get("Pages")
            .and_then(|p| p.as_reference())
            .ok_or_else(|| Error::InvalidPdf("Catalog missing /Pages entry".to_string()))?;

        let mut out: Vec<ObjectRef> = Vec::new();
        let mut visited: HashSet<ObjectRef> = HashSet::new();
        self.collect_page_refs(pages_ref, &mut out, &mut visited)?;
        Ok(out)
    }

    pub(super) fn collect_page_refs(
        &self,
        node_ref: ObjectRef,
        out: &mut Vec<ObjectRef>,
        visited: &mut HashSet<ObjectRef>,
    ) -> Result<()> {
        if !visited.insert(node_ref) {
            return Ok(());
        }
        let node = self.load_object(node_ref)?;
        let dict = match node.as_dict() {
            Some(d) => d,
            None => return Ok(()),
        };

        let kids = match dict.get("Kids").and_then(|k| k.as_array()) {
            Some(k) => k,
            None => {
                // Leaf reached (no /Kids — assume Page).
                out.push(node_ref);
                return Ok(());
            }
        };

        // Fast path: flat subtree — every kid is a leaf when /Count == kids.len().
        let count = dict.get("Count").and_then(|c| c.as_integer()).unwrap_or(-1);
        if count >= 0 && (count as usize) == kids.len() {
            for kid in kids {
                if let Some(kid_ref) = kid.as_reference() {
                    out.push(kid_ref);
                }
            }
            return Ok(());
        }

        // Mixed tree — recurse into each kid.
        for kid in kids {
            if let Some(kid_ref) = kid.as_reference() {
                self.collect_page_refs(kid_ref, out, visited)?;
            }
        }
        Ok(())
    }
}
