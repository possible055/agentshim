use super::*;

impl FontInfo {
    /// Handles both named encodings (e.g., /WinAnsiEncoding) and encoding dictionaries
    /// with /Differences arrays that override specific character codes.
    ///
    /// # PDF Spec Reference
    ///
    /// ISO 32000-1:2008, Section 9.6.6.2 - Character Encoding
    ///
    /// A /Differences array has the format:
    /// ```pdf
    /// /Encoding <<
    ///     /BaseEncoding /WinAnsiEncoding
    ///     /Differences [code1 /name1 /name2 ... codeN /nameN ...]
    /// >>
    /// ```
    ///
    /// Where integers specify starting codes, and names specify glyphs for consecutive codes.
    ///
    /// The third element of the returned tuple is the `diff_glyph_names` side
    /// map: `code → /Differences glyph name` for simple fonts (empty otherwise).
    /// It retains the authoritative glyph *name* (not the resolved char) so the
    /// punctuation-recovery interceptions in `char_to_unicode` can consult it.
    pub(super) fn parse_encoding(
        enc_obj: &Object,
        doc: &PdfDocument,
        font_program_encoding: Option<&HashMap<u8, char>>,
    ) -> Result<(Encoding, HashMap<u8, String>, HashMap<u8, String>)> {
        let empty_map = HashMap::new();
        // Encoding can be either a name or a dictionary
        if let Some(name) = enc_obj.as_name() {
            // Standard encoding names (no /Differences ⇒ no glyph-name side map)
            match name {
                "WinAnsiEncoding" => Ok((
                    Encoding::Standard("WinAnsiEncoding".to_string()),
                    empty_map,
                    HashMap::new(),
                )),
                "MacRomanEncoding" => Ok((
                    Encoding::Standard("MacRomanEncoding".to_string()),
                    empty_map,
                    HashMap::new(),
                )),
                "MacExpertEncoding" => Ok((
                    Encoding::Standard("MacExpertEncoding".to_string()),
                    empty_map,
                    HashMap::new(),
                )),
                "Identity-H" | "Identity-V" => Ok((Encoding::Identity, empty_map, HashMap::new())),
                _ => Ok((
                    Encoding::Standard(name.to_string()),
                    empty_map,
                    HashMap::new(),
                )),
            }
        } else if let Some(dict) = enc_obj.as_dict() {
            // Check if this is a CMap stream (Type0 font encoding reference)
            // Per PDF Spec §9.7.5.2, Type0 fonts can reference a CMap stream
            // via /Encoding. For known Adobe character collections (Japan1, GB1,
            // CNS1, Korea1), these define charcode→CID identity mappings and we
            // can resolve CIDs via predefined CID-to-Unicode tables.
            // For custom CMaps (e.g., "Prince-ArialMT-H"), we preserve the default
            // behavior since we can't parse arbitrary CMap programs yet.
            if let Some(cmap_name) = dict.get("CMapName").and_then(|n| n.as_name()) {
                // Check for Adobe standard character collection CMaps.
                // These are named like "Adobe-Japan1-2", "Adobe-Korea1-0", etc.
                // For these collections, charcode→CID is identity, and we can
                // resolve CID→Unicode via predefined tables.
                let is_adobe_collection = cmap_name.starts_with("Adobe-")
                    && (cmap_name.contains("Japan")
                        || cmap_name.contains("GB")
                        || cmap_name.contains("CNS")
                        || cmap_name.contains("Korea"));
                if is_adobe_collection {
                    log::debug!(
                        "Encoding is Adobe CMap stream (CMapName={:?}), treating as Identity",
                        cmap_name
                    );
                    return Ok((Encoding::Identity, HashMap::new(), HashMap::new()));
                }
                // For predefined PDF CMaps like "Identity-H", "Identity-V"
                if cmap_name == "Identity-H" || cmap_name == "Identity-V" {
                    return Ok((Encoding::Identity, HashMap::new(), HashMap::new()));
                }
                // Custom CMap streams (e.g., "Prince-ArialMT-H", "OneByteIdentityH")
                log::debug!(
                    "Encoding is custom CMap stream (CMapName={:?}), treating as Standard",
                    cmap_name
                );
                return Ok((
                    Encoding::Standard(cmap_name.to_string()),
                    HashMap::new(),
                    HashMap::new(),
                ));
            }

            // Custom encoding dictionary - parse /Differences array
            let mut multi_char_map: HashMap<u8, String> = HashMap::new();
            // Retain the raw /Differences glyph name per code (see field docs).
            let mut diff_glyph_names: HashMap<u8, String> = HashMap::new();

            // Step 1: Get base encoding (if specified)
            let mut encoding_map: HashMap<u8, char> = if let Some(base_enc_obj) =
                dict.get("BaseEncoding")
            {
                // Resolve indirect reference for /BaseEncoding
                let resolved_base = if let Some(obj_ref) = base_enc_obj.as_reference() {
                    doc.load_object(obj_ref).ok()
                } else {
                    None
                };
                let base_obj = resolved_base.as_ref().unwrap_or(base_enc_obj);

                if let Some(base_name) = base_obj.as_name() {
                    // Build initial encoding from base encoding
                    let mut map = HashMap::new();
                    for code in 0u8..=255 {
                        if let Some(unicode_str) = standard_encoding_lookup(base_name, code) {
                            // Convert the first character of the unicode string
                            if let Some(ch) = unicode_str.chars().next() {
                                map.insert(code, ch);
                            }
                        }
                    }
                    map
                } else {
                    HashMap::new()
                }
            } else if let Some(prog_enc) = font_program_encoding {
                // PDF Spec ISO 32000-1:2008, Section 9.6.6.1:
                // "If BaseEncoding is absent and the font has a built-in encoding,
                // the built-in encoding shall be used as the base encoding."
                prog_enc.clone()
            } else {
                // No base encoding specified and no font program - use StandardEncoding as default
                let mut map = HashMap::new();
                for code in 0u8..=255 {
                    if let Some(unicode_str) = standard_encoding_lookup("StandardEncoding", code) {
                        if let Some(ch) = unicode_str.chars().next() {
                            map.insert(code, ch);
                        }
                    }
                }
                map
            };

            // Step 2: Apply /Differences array if present
            if let Some(differences_obj) = dict.get("Differences") {
                log::info!("Found /Differences array in encoding dictionary");

                // Resolve indirect reference for /Differences itself
                let resolved_diff = if let Some(obj_ref) = differences_obj.as_reference() {
                    doc.load_object(obj_ref).ok()
                } else {
                    None
                };
                let diff_obj = resolved_diff.as_ref().unwrap_or(differences_obj);

                if let Some(diff_array) = diff_obj.as_array() {
                    log::info!("/Differences array has {} items", diff_array.len());
                    let mut current_code: u32 = 0;

                    for item in diff_array {
                        // Resolve indirect references within the array
                        let resolved_item = if let Some(obj_ref) = item.as_reference() {
                            doc.load_object(obj_ref).ok()
                        } else {
                            None
                        };
                        let actual_item = resolved_item.as_ref().unwrap_or(item);

                        match actual_item {
                            Object::Integer(code) => {
                                // New starting code
                                current_code = *code as u32;
                            }
                            Object::Name(glyph_name) => {
                                // Retain the authoritative glyph name for this code
                                // (ISO 32000-1 §9.6.6.1, Table 114). Kept regardless
                                // of whether it resolves to a single/compound/unknown
                                // Unicode value, so the punctuation-recovery
                                // interceptions in `char_to_unicode` can consult it.
                                if current_code <= 255 {
                                    diff_glyph_names.insert(current_code as u8, glyph_name.clone());
                                }
                                // Map glyph name to Unicode character(s)
                                if let Some(unicode_char) = glyph_name_to_unicode(glyph_name) {
                                    if current_code <= 255 {
                                        encoding_map.insert(current_code as u8, unicode_char);
                                        if is_ligature_char(unicode_char) {
                                            log::info!(
                                                "/Differences: code {} → /{} → '{}' (U+{:04X})",
                                                current_code,
                                                glyph_name,
                                                unicode_char,
                                                unicode_char as u32
                                            );
                                        }
                                    }
                                } else if let Some(unicode_string) =
                                    glyph_name_to_unicode_string(glyph_name)
                                {
                                    // Compound glyph name (e.g. f_f → "ff", f_f_i → "ffi")
                                    if current_code <= 255 {
                                        multi_char_map
                                            .insert(current_code as u8, unicode_string.clone());
                                        log::info!(
                                            "/Differences: code {} → /{} → {:?} (compound)",
                                            current_code,
                                            glyph_name,
                                            unicode_string
                                        );
                                    }
                                } else {
                                    log::debug!(
                                        "Unknown glyph name '{}' at code {} in /Differences array",
                                        glyph_name,
                                        current_code
                                    );
                                }
                                current_code += 1;
                            }
                            _ => {
                                // Invalid item in /Differences array - skip
                                log::warn!(
                                    "Unexpected item in /Differences array: {:?}",
                                    actual_item
                                );
                            }
                        }
                    }

                    log::debug!(
                        "Parsed /Differences array with {} custom mappings",
                        encoding_map.len()
                    );
                } else {
                    log::warn!("/Differences is not an array: {:?}", diff_obj);
                }
            }

            if !encoding_map.is_empty() || !multi_char_map.is_empty() {
                Ok((
                    Encoding::Custom(encoding_map),
                    multi_char_map,
                    diff_glyph_names,
                ))
            } else {
                Ok((
                    Encoding::Standard("StandardEncoding".to_string()),
                    HashMap::new(),
                    diff_glyph_names,
                ))
            }
        } else {
            Ok((
                Encoding::Standard("StandardEncoding".to_string()),
                HashMap::new(),
                HashMap::new(),
            ))
        }
    }
}
