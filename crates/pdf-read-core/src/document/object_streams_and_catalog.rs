use super::parsing::*;
use super::preflight::*;
use super::*;

impl PdfDocument {
    /// Implementation with recursion guard to prevent infinite loops.
    pub(super) fn load_uncompressed_object_impl(
        &self,
        obj_ref: ObjectRef,
        offset: u64,
        already_corrected: bool,
    ) -> Result<Object> {
        // --- Phase 1: read the object header under a single lock guard ---
        // Holding one guard for seek+read prevents the split-lock race (#398 Race A)
        // where a concurrent thread can re-seek the shared BufReader between our
        // seek() and read_until() calls.
        let (header_bytes, full_header) = {
            let mut reader = self.reader.lock_or_recover();
            reader.seek(SeekFrom::Start(offset))?;

            // Read bytes for object header (e.g., "1 0 obj")
            let mut header_bytes = Vec::new();
            let bytes_read = reader.read_until(b'\n', &mut header_bytes)?;

            if bytes_read == 0 {
                let msg = format!("Unexpected EOF while reading object {} header", obj_ref.id);
                log::warn!("{}", msg);
                // also push into structured sink so
                // callers can retrieve as data via flatten_warnings.
                self.push_structured_warning(crate::extractors::warnings::Warning {
                    category: crate::extractors::warnings::WarningCategory::EofPremature,
                    page: None,
                    message: msg,
                    spec_section: Some("7.5"),
                });
                return Err(Error::UnexpectedEof);
            }

            let line = String::from_utf8_lossy(&header_bytes);

            // Issue #45: Handle multi-line object headers
            let mut full_header = line.to_string();
            let max_header_lines = 5;
            let mut lines_read = 1;

            while !has_standalone_obj_keyword(&full_header) && lines_read < max_header_lines {
                let mut next_bytes = Vec::new();
                let next_read = reader.read_until(b'\n', &mut next_bytes)?;
                if next_read == 0 {
                    break;
                }
                let next_line = String::from_utf8_lossy(&next_bytes);
                full_header.push(' ');
                full_header.push_str(&next_line);
                lines_read += 1;
            }
            // Reader guard drops here — before any recursive fallback calls.
            (header_bytes, full_header)
        };

        // Verify object header format
        // Split by whitespace to handle various formats (single-line or multi-line)
        let parts: Vec<&str> = full_header.split_whitespace().collect();

        // Find standalone "obj" keyword (not "endobj")
        let obj_pos = parts
            .iter()
            .position(|&p| p == "obj" || (p.starts_with("obj") && !p.starts_with("endobj")));

        // Validate object header has proper format: <id> <gen> obj
        let obj_pos = match obj_pos {
            Some(pos) if pos >= 2 => pos,
            _ => {
                // Only try backwards search once to prevent infinite recursion
                if !already_corrected {
                    // xref offset might be incorrect (pointing to object body instead of header)
                    // Try searching backwards for the object header
                    log::debug!(
                        "No object header at offset {}, searching backwards for object {} {} obj",
                        offset,
                        obj_ref.id,
                        obj_ref.gen
                    );

                    if let Ok(corrected_offset) = self.find_object_header_backwards(obj_ref, offset)
                    {
                        log::info!(
                            "Found object header at offset {} (xref said {})",
                            corrected_offset,
                            offset
                        );
                        return self.load_uncompressed_object_impl(obj_ref, corrected_offset, true);
                    }
                }

                log::warn!(
                    "Malformed object header at offset {}: {}",
                    offset,
                    full_header.trim()
                );
                return Err(Error::ParseError {
                    offset: offset as usize,
                    reason: format!("Expected object header, found: {}", full_header.trim()),
                });
            }
        };

        let _obj_pos = obj_pos;

        // Parse the object number and generation from header. If either
        // fails to parse as a number, the xref-reported offset is pointing
        // into the middle of a previous object's tail (e.g. xref says 12345
        // but the real `N G obj` header starts at 12348 because three bytes
        // of CR/LF/terminator got mis-accounted for by the producer — a
        // pattern seen in the wild). Fall back to the whole-file scan
        // cache: if scan recorded a different offset for this id, retry
        // from there before giving up.
        let obj_num_parsed = parts[0].parse::<u32>();
        let gen_num_parsed = parts[1].parse::<u16>();
        if !already_corrected && (obj_num_parsed.is_err() || gen_num_parsed.is_err()) {
            if let Ok(scan_offset) = self.scan_for_object(obj_ref) {
                if scan_offset != offset {
                    log::debug!(
                        "Header parse failed at xref offset {} (parts[0]={:?}); retrying at scan-reported offset {}",
                        offset,
                        parts[0],
                        scan_offset
                    );
                    return self.load_uncompressed_object_impl(obj_ref, scan_offset, true);
                }
            }
        }
        let obj_num: u32 = obj_num_parsed.map_err(|_| Error::ParseError {
            offset: offset as usize,
            reason: format!("Invalid object number in header: {}", parts[0]),
        })?;
        let gen_num: u16 = gen_num_parsed.map_err(|_| Error::ParseError {
            offset: offset as usize,
            reason: format!("Invalid generation number in header: {}", parts[1]),
        })?;

        // Verify object reference matches (warn but don't fail on mismatch)
        if obj_num != obj_ref.id || gen_num != obj_ref.gen {
            log::warn!(
                "Object reference mismatch at offset {}: expected {} {} obj, found {} {} obj",
                offset,
                obj_ref.id,
                obj_ref.gen,
                obj_num,
                gen_num
            );
        }

        // Check if there's content after "obj" on the same line
        // Some PDFs have "N G obj\n<<..." while others have "N G obj<<..." on one line
        let mut data = Vec::new();

        // Find where "obj" ends in the original bytes
        // We need to include anything after "obj" in the header line
        if let Some(obj_keyword_pos) = header_bytes.windows(3).position(|w| w == b"obj") {
            let after_obj_pos = obj_keyword_pos + 3; // "obj" is 3 bytes

            // Skip whitespace after "obj"
            let mut content_start = after_obj_pos;
            while content_start < header_bytes.len()
                && (header_bytes[content_start] == b' '
                    || header_bytes[content_start] == b'\t'
                    || header_bytes[content_start] == b'\r')
            {
                content_start += 1;
            }

            // If there's a newline, skip it (normal case: "N G obj\n")
            // If there's content (like "<<"), include it (malformed case: "N G obj<<...")
            if content_start < header_bytes.len() && header_bytes[content_start] != b'\n' {
                // There's content on the same line after "obj" - include it
                data.extend_from_slice(&header_bytes[content_start..]);
                log::debug!(
                    "Object {} has content after 'obj' on header line ({} bytes)",
                    obj_ref.id,
                    header_bytes.len() - content_start
                );
            }
        }

        // --- Phase 2: read body under a single lock guard (#398 Race A) ---
        // Use byte limit instead of line count — large uncompressed streams can have
        // hundreds of thousands of short lines (e.g., vector path drawing commands).
        const MAX_BYTES: usize = 100 * 1024 * 1024; // 100 MB safety limit

        {
            let mut reader = self.reader.lock_or_recover();
            loop {
                let mut chunk = Vec::new();
                let bytes_read = reader.read_until(b'\n', &mut chunk)?;

                if data.len() > MAX_BYTES {
                    log::warn!(
                        "Object {} exceeded maximum byte limit ({} bytes), truncating",
                        obj_ref.id,
                        MAX_BYTES
                    );
                    break;
                }

                if bytes_read == 0 {
                    let msg = format!(
                        "Unexpected EOF while reading object {} (no endobj found after {} bytes)",
                        obj_ref.id,
                        data.len()
                    );
                    log::warn!("{}", msg);
                    // structured-warnings sink.
                    self.push_structured_warning(crate::extractors::warnings::Warning {
                        category: crate::extractors::warnings::WarningCategory::EofPremature,
                        page: None,
                        message: msg,
                        spec_section: Some("7.5"),
                    });
                    // Don't fail - try to parse what we have
                    break;
                }

                // Check if we reached endobj
                if chunk.contains(&b'e') {
                    // Find "endobj" in the chunk (working with bytes, not chars)
                    if let Some(endobj_pos) = find_substring(&chunk, b"endobj") {
                        // Include everything before "endobj" but not "endobj" itself
                        data.extend_from_slice(&chunk[..endobj_pos]);
                        break;
                    }
                }

                data.extend_from_slice(&chunk);
            }
        }

        // Parse the object data
        log::debug!(
            "About to parse object {} gen {} ({} bytes)",
            obj_ref.id,
            obj_ref.gen,
            data.len()
        );

        // Corrupted objects degrade to Null so extraction can continue on
        // partial PDFs rather than aborting.
        let mut obj = match parse_object(&data) {
            Ok((_, parsed_obj)) => parsed_obj,
            Err(e) => {
                let error_kind = match &e {
                    nom::Err::Incomplete(_) => "Incomplete data",
                    nom::Err::Error(err) | nom::Err::Failure(err) => match err.code {
                        nom::error::ErrorKind::Eof => "Unexpected EOF",
                        nom::error::ErrorKind::Tag => "Expected tag not found",
                        nom::error::ErrorKind::Fail => "Parse failed",
                        _ => "Parse error",
                    },
                };
                log::warn!(
                    "Object {} at offset {} is corrupted ({}), using Null placeholder. \
                     This may result in missing content from the PDF.",
                    obj_ref.id,
                    offset,
                    error_kind
                );
                Object::Null
            }
        };

        // Decrypt string values inside this uncompressed object before
        // caching. Skip the /Encrypt dict (its entries are key material)
        // and the non-authenticated case (no key derived yet). Strings
        // inside compressed objects ride along with the ObjStm payload
        // and are already in clear text per ISO 32000-1:2008 §7.6.2.
        let is_encrypt_dict = *self.encrypt_dict_ref.lock_or_recover() == Some(obj_ref);
        if !is_encrypt_dict {
            let handler_guard = self.encryption_handler.lock_or_recover();
            if let Some(handler) = handler_guard.as_ref() {
                if handler.is_authenticated() {
                    Self::decrypt_strings_in_object(
                        handler,
                        &mut obj,
                        obj_ref.id,
                        obj_ref.gen as u32,
                    );
                }
            }
        }

        // Cache the object
        self.object_cache
            .lock_or_recover()
            .insert(obj_ref, obj.clone());

        Ok(obj)
    }

    /// Load a compressed object from an object stream (Type 2 xref entry).
    ///
    /// # Arguments
    ///
    /// * `obj_ref` - The object reference being loaded
    /// * `stream_obj_num` - The object number of the object stream
    /// * `index_in_stream` - The index within the stream (unused but provided for completeness)
    pub(super) fn load_compressed_object(
        &self,
        obj_ref: ObjectRef,
        stream_obj_num: u32,
        _index_in_stream: u16,
    ) -> Result<Object> {
        use crate::objstm::parse_object_stream_with_decryption;

        log::debug!(
            "[load_compressed_debug] Loading obj {} from stream {}",
            obj_ref.id,
            stream_obj_num
        );

        // Per PDF §7.6.3, object streams (/Type /ObjStm) shall NOT be individually
        // encrypted. Encryption initialization is therefore not required to read an
        // ObjStm: the unencrypted parse path below is always attempted first. If
        // initialization fails (e.g. unsupported algorithm, no legacy-crypto feature),
        // log and continue — the handler will be None and we'll use the no-decryption
        // path, which is exactly what the spec mandates for ObjStm content.
        if let Err(e) = self.ensure_encryption_initialized() {
            log::debug!(
                "Encryption init skipped for ObjStm {} load ({}); will parse without decryption",
                stream_obj_num,
                e
            );
        }

        // Load the object stream
        let stream_ref = ObjectRef::new(stream_obj_num, 0);
        let stream_obj = self.load_uncompressed_object(stream_ref, {
            // Look up the stream's offset in the xref table
            let stream_entry = match self.xref.get(stream_obj_num) {
                Some(entry) => entry,
                None => {
                    // PDF Spec §7.3.10: treat as null
                    log::warn!(
                        "Object stream {} not in xref, treating compressed object {} as Null",
                        stream_obj_num,
                        obj_ref.id
                    );
                    self.object_cache
                        .lock_or_recover()
                        .insert(obj_ref, Object::Null);
                    return Ok(Object::Null);
                }
            };

            if stream_entry.entry_type != crate::xref::XRefEntryType::Uncompressed {
                return Err(Error::InvalidPdf(format!(
                    "object stream {} is not an uncompressed object",
                    stream_obj_num
                )));
            }

            stream_entry.offset
        })?;

        // Parse all objects from the stream.
        //
        // Per ISO 32000-2:2020 Section 7.6.3, object streams (/Type /ObjStm)
        // cross-reference streams (/Type /XRef) shall NOT be individually encrypted.
        // The stream data is only compressed, not encrypted. Many PDF producers
        // (including many real-world producers) follow this rule even under
        // PDF 1.x, so attempting AES decryption on the raw stream bytes fails
        // because the data length is not a multiple of the AES block size (16).
        //
        // We therefore always parse object streams WITHOUT decryption. If a
        // future PDF is encountered where the producer DID encrypt the ObjStm
        // (non-standard), the unencrypted parse will fail and we fall back to
        // trying with decryption.
        let handler_ref = self.encryption_handler.lock_or_recover();
        let objects_map = if handler_ref.is_some() {
            // First try without decryption (spec-compliant path)
            match parse_object_stream_with_decryption(&stream_obj, None, 0, 0) {
                Ok(map) => map,
                Err(_no_decrypt_err) => {
                    // Fallback: try with decryption for non-standard producers
                    let handler = handler_ref.as_ref().unwrap();
                    let decrypt_fn = |data: &[u8]| -> Result<Vec<u8>> {
                        handler.decrypt_stream(data, stream_obj_num, 0)
                    };
                    parse_object_stream_with_decryption(
                        &stream_obj,
                        Some(&decrypt_fn),
                        stream_obj_num,
                        0,
                    )?
                }
            }
        } else {
            parse_object_stream_with_decryption(&stream_obj, None, 0, 0)?
        };
        drop(handler_ref);

        // Extract the requested object
        let obj = match objects_map.get(&obj_ref.id) {
            Some(o) => o.clone(),
            None => {
                // PDF Spec §7.3.10: treat as null
                log::warn!(
                    "Object {} not found in object stream {}, treating as Null",
                    obj_ref.id,
                    stream_obj_num
                );
                Object::Null
            }
        };

        // Cache all objects from the stream for future access
        // IMPORTANT: Only cache objects whose xref entry points to THIS stream.
        // In incremental updates, the same object number may exist in multiple streams,
        // and we must not cache a stale version from an older stream.
        for (obj_num, object) in objects_map {
            let cache_ref = ObjectRef::new(obj_num, 0);
            let should_cache = if let Some(entry) = self.xref.get(obj_num) {
                // Only cache if the xref says this object belongs to this stream
                entry.entry_type == crate::xref::XRefEntryType::Compressed
                    && entry.offset == stream_obj_num as u64
            } else {
                // Object not in xref at all -- safe to cache as it's only in this stream
                true
            };
            if should_cache {
                self.object_cache
                    .lock_or_recover()
                    .insert(cache_ref, object);
            } else {
                log::debug!(
                    "[cache_debug] NOT caching obj {} from stream {} (xref points elsewhere)",
                    obj_num,
                    stream_obj_num
                );
            }
        }

        Ok(obj)
    }

    /// Find object header by searching backwards from a given offset.
    ///
    /// Some PDF generators create xref tables with incorrect offsets that point
    /// to the object body instead of the header. This function searches backwards
    /// from the xref offset to find the actual "N G obj" header.
    ///
    /// We search up to 100 bytes backwards, looking for a line that matches
    /// the expected object header format.
    pub(super) fn find_object_header_backwards(
        &self,
        obj_ref: ObjectRef,
        wrong_offset: u64,
    ) -> Result<u64> {
        // Don't search before the start of the file
        if wrong_offset == 0 {
            return Err(Error::ParseError {
                offset: wrong_offset as usize,
                reason: "Cannot search backwards from offset 0".to_string(),
            });
        }

        // Search up to 100 bytes backwards (reasonable for most PDFs)
        let search_distance = std::cmp::min(100, wrong_offset);
        let search_start = wrong_offset - search_distance;

        // Read the search region under one lock guard (#398 Race A).
        let mut buffer = vec![0u8; search_distance as usize + 100]; // Extra bytes to read full line
        let bytes_read = {
            let mut reader = self.reader.lock_or_recover();
            reader.seek(SeekFrom::Start(search_start))?;
            reader.read(&mut buffer)?
        };

        if bytes_read == 0 {
            return Err(Error::ParseError {
                offset: wrong_offset as usize,
                reason: "Could not read backwards search region".to_string(),
            });
        }

        // Build the expected header pattern as bytes (NOT string to avoid UTF-8 corruption)
        let expected_header = format!("{} {} obj", obj_ref.id, obj_ref.gen);
        let pattern_bytes = expected_header.as_bytes();

        // Search for the byte pattern directly (avoids UTF-8 conversion issues with binary data)
        // Find the match closest to wrong_offset (prefer before, but allow small offsets after)
        let mut best_match: Option<(usize, i64)> = None; // (position, distance_from_wrong)

        for (i, window) in buffer[..bytes_read]
            .windows(pattern_bytes.len())
            .enumerate()
        {
            if window == pattern_bytes {
                let candidate_offset = search_start + i as u64;
                let distance = (candidate_offset as i64) - (wrong_offset as i64);

                // Accept matches within -100 to +10 bytes of wrong_offset
                // (xref might be slightly off by a few bytes)
                if (-100..=10).contains(&distance) {
                    // Prefer the match closest to wrong_offset
                    let is_better = best_match
                        .as_ref()
                        .is_none_or(|(_, best_dist)| distance.abs() < best_dist.abs());

                    if is_better {
                        best_match = Some((i, distance));
                    }
                }
            }
        }

        if let Some((pos, distance)) = best_match {
            let absolute_offset = search_start + pos as u64;
            log::debug!(
                "Found object header '{}' at offset {} ({:+} bytes from xref at {})",
                expected_header,
                absolute_offset,
                distance,
                wrong_offset
            );
            return Ok(absolute_offset);
        }

        // Try with whitespace variations (space, double-space, tab between obj_id and gen)
        let patterns = [
            format!("{} {} obj", obj_ref.id, obj_ref.gen).into_bytes(),
            format!("{}  {} obj", obj_ref.id, obj_ref.gen).into_bytes(),
            format!("{}\t{} obj", obj_ref.id, obj_ref.gen).into_bytes(),
            format!("{} {}\tobj", obj_ref.id, obj_ref.gen).into_bytes(),
        ];

        for pattern in &patterns {
            let mut best_match: Option<(usize, i64)> = None;

            for (i, window) in buffer[..bytes_read].windows(pattern.len()).enumerate() {
                if window == pattern.as_slice() {
                    let candidate_offset = search_start + i as u64;
                    let distance = (candidate_offset as i64) - (wrong_offset as i64);

                    if (-100..=10).contains(&distance) {
                        let is_better = best_match
                            .as_ref()
                            .is_none_or(|(_, best_dist)| distance.abs() < best_dist.abs());

                        if is_better {
                            best_match = Some((i, distance));
                        }
                    }
                }
            }

            if let Some((pos, distance)) = best_match {
                let absolute_offset = search_start + pos as u64;
                log::debug!(
                    "Found object header '{}' at offset {} ({:+} bytes, pattern match)",
                    expected_header,
                    absolute_offset,
                    distance
                );
                return Ok(absolute_offset);
            }
        }

        Err(Error::ParseError {
            offset: wrong_offset as usize,
            reason: format!(
                "Could not find object header '{}' within {} bytes before offset",
                expected_header, search_distance
            ),
        })
    }

    /// Get the document catalog (root object).
    ///
    /// The catalog is the root of the document's object hierarchy.
    /// It contains references to the page tree, outlines, etc.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The /Root entry is present but is not a reference
    /// - Loading the catalog object fails
    /// - The trailer omits /Root **and** no `/Type /Catalog` object can be
    ///   found by scanning (the issue #509 recovery path: a missing /Root is
    ///   not itself fatal — the Catalog is discovered by object scan, as
    ///   Poppler / PDFium do — but it does error if that scan also fails)
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use pdf_oxide::document::PdfDocument;
    /// # let mut doc = PdfDocument::open("sample.pdf")?;
    /// let catalog = doc.catalog()?;
    /// # Ok::<(), pdf_oxide::error::Error>(())
    /// ```
    pub fn catalog(&self) -> Result<Object> {
        let trailer_dict = self
            .trailer
            .as_dict()
            .ok_or_else(|| Error::InvalidPdf("Trailer is not a dictionary".to_string()))?;

        if let Some(root_obj) = trailer_dict.get("Root") {
            let root_ref = root_obj
                .as_reference()
                .ok_or_else(|| Error::InvalidPdf("/Root is not a reference".to_string()))?;
            return self.load_object(root_ref);
        }

        // The trailer omits /Root. A Linearized file's sparse end-of-file
        // trailer legitimately does this; discover the Catalog
        // by scanning indirect objects for /Type /Catalog, as Poppler /
        // PDFium do.
        self.find_catalog_by_scan().ok_or_else(|| {
            Error::InvalidPdf(
                "Trailer omits /Root and no /Type /Catalog object could be found by scanning"
                    .to_string(),
            )
        })
    }

    /// Scan indirect objects for the document Catalog (`/Type /Catalog`).
    ///
    /// Used only as a fallback when the trailer omits `/Root`.
    /// Bounded so a pathological xref can't turn this into an unbounded
    /// scan; the Catalog is virtually always one of the first objects.
    ///
    /// The smallest `MAX_SCAN` object numbers are scanned, ascending.
    /// `all_object_numbers()` is `HashMap`-backed, so iterating it directly
    /// would be nondeterministic — a bounded scan over an arbitrary subset
    /// can miss the Catalog on different runs. `smallest_object_numbers`
    /// makes discovery deterministic, scans low-numbered objects first
    /// (where the Catalog conventionally lives), and bounds the candidate
    /// set *before* sorting so a pathological xref stays O(n log MAX_SCAN).
    pub(super) fn find_catalog_by_scan(&self) -> Option<Object> {
        const MAX_SCAN: usize = 4096;
        let nums = self.xref.smallest_object_numbers(MAX_SCAN);
        let mut checked = 0usize;
        for num in nums {
            if checked >= MAX_SCAN {
                break;
            }
            let generation = match self.xref.get(num) {
                Some(e) if e.in_use => e.generation,
                _ => continue,
            };
            checked += 1;
            if let Ok(obj) = self.load_object(ObjectRef::new(num, generation)) {
                if obj
                    .as_dict()
                    .and_then(|d| d.get("Type"))
                    .and_then(|t| t.as_name())
                    == Some("Catalog")
                {
                    log::info!(
                        "Catalog discovered by object scan: {} {} obj",
                        num,
                        generation
                    );
                    return Some(obj);
                }
            }
        }
        None
    }

    /// Get the structure tree (logical structure) of the document.
    ///
    /// Tagged PDFs contain a structure tree that defines the logical structure
    /// and reading order of the document. This is the PDF-spec-compliant way
    /// to determine reading order.
    ///
    /// Returns `Ok(Some(StructTreeRoot))` if the document has a structure tree,
    /// `Ok(None)` if it's not a tagged PDF, or an error if parsing fails.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use pdf_oxide::document::PdfDocument;
    /// # let mut doc = PdfDocument::open("sample.pdf")?;
    /// if let Some(struct_tree) = doc.structure_tree()? {
    ///     println!("This is a Tagged PDF with logical structure");
    /// } else {
    ///     println!("This PDF does not have a structure tree");
    /// }
    /// # Ok::<(), pdf_oxide::error::Error>(())
    /// ```
    pub fn structure_tree(&self) -> Result<Option<crate::structure::StructTreeRoot>> {
        crate::structure::parse_structure_tree(self)
    }

    /// Returns the document's structure tree, bounding the parse work by an
    /// optional wall-clock `budget`.
    ///
    /// `None` parses the complete tree (identical to [`Self::structure_tree`]);
    /// `Some(duration)` returns `Ok(None)` if parsing exceeds that budget, so a
    /// latency-sensitive caller can fall back to another strategy. Prefer `None`
    /// unless you have a concrete responsiveness requirement.
    pub fn structure_tree_with_budget(
        &self,
        budget: Option<std::time::Duration>,
    ) -> Result<Option<crate::structure::StructTreeRoot>> {
        crate::structure::parse_structure_tree_with_budget(self, budget)
    }
}
