use super::*;

impl FontInfo {
    /// Parse encoding from an encoding object.
    ///
    /// Phase 3: Parse CIDSystemInfo from CIDFont dictionary
    /// Extracts Registry, Ordering, and Supplement for character collection identification
    /// Per PDF Spec ISO 32000-1:2008, Section 9.7.3
    pub(super) fn parse_cidsysteminfo(
        cidfont_dict: &HashMap<String, Object>,
        doc: &PdfDocument,
    ) -> Result<CIDSystemInfo> {
        let sysinfo_obj = cidfont_dict
            .get("CIDSystemInfo")
            .ok_or_else(|| Error::ParseError {
                offset: 0,
                reason: "CIDFont missing required /CIDSystemInfo entry".to_string(),
            })?;

        // Resolve reference if needed
        let resolved = if let Some(ref_obj) = sysinfo_obj.as_reference() {
            doc.load_object(ref_obj)?
        } else {
            sysinfo_obj.clone()
        };

        let sysinfo_dict = resolved.as_dict().ok_or_else(|| Error::ParseError {
            offset: 0,
            reason: "CIDSystemInfo is not a dictionary".to_string(),
        })?;

        let registry = sysinfo_dict
            .get("Registry")
            .and_then(|obj| obj.as_string())
            .map(|s| String::from_utf8_lossy(s).to_string())
            .unwrap_or_else(|| "Unknown".to_string());

        let ordering = sysinfo_dict
            .get("Ordering")
            .and_then(|obj| obj.as_string())
            .map(|s| String::from_utf8_lossy(s).to_string())
            .unwrap_or_else(|| "Unknown".to_string());

        let supplement = sysinfo_dict
            .get("Supplement")
            .and_then(|obj| obj.as_integer())
            .unwrap_or(0) as i32;

        log::debug!(
            "CIDSystemInfo parsed: Registry={}, Ordering={}, Supplement={}",
            registry,
            ordering,
            supplement
        );

        Ok(CIDSystemInfo {
            registry,
            ordering,
            supplement,
        })
    }

    /// Phase 3: Parse DescendantFonts array for Type0 fonts
    /// Extracts CIDFont dictionary and related information
    /// Per PDF Spec ISO 32000-1:2008, Section 9.7.1
    ///
    /// Returns: (CIDToGIDMap, CIDSystemInfo, CIDFontType, CIDWidths, DefaultWidth,
    ///          has_explicit_dw, TrueTypeCMap, (has_font_program, EmbeddedFontData),
    ///          raw_ascent, raw_descent, vertical_metrics, dw2)
    ///
    /// The embedded-font element pairs descriptor `/FontFile{,2,3}` key
    /// presence with the extracted bytes so callers can tell "no program"
    /// apart from "program present but failed to load/decode".
    #[allow(clippy::type_complexity)] // tuple grew incrementally; refactor deferred to a follow-up
    pub(super) fn parse_descendant_fonts(
        font_dict: &HashMap<String, Object>,
        base_font: &str,
        doc: &PdfDocument,
    ) -> Result<(
        Option<CIDToGIDMap>,
        Option<CIDSystemInfo>,
        Option<String>,
        Option<HashMap<u16, f32>>,
        f32,                                   // cid_default_width
        bool,                                  // has_explicit_dw (F14/F15 fix)
        Option<TrueTypeCMap>,                  // TrueType cmap from descendant's embedded font
        (bool, Option<Arc<Vec<u8>>>),          // (/FontFile{,2,3} key present, extracted data)
        Option<f32>,                           // raw_ascent from descendant FontDescriptor
        Option<f32>,                           // raw_descent from descendant FontDescriptor
        Option<HashMap<u16, VerticalMetrics>>, // /W2 per-CID vertical metrics
        VerticalMetrics,                       // /DW2 default vertical metrics (or spec defaults)
    )> {
        let descendant_obj = font_dict
            .get("DescendantFonts")
            .ok_or_else(|| Error::ParseError {
                offset: 0,
                reason: format!(
                    "Type0 font '{}' missing required /DescendantFonts entry",
                    base_font
                ),
            })?;

        // Resolve reference if needed
        let resolved = if let Some(ref_obj) = descendant_obj.as_reference() {
            doc.load_object(ref_obj)?
        } else {
            descendant_obj.clone()
        };

        let array = resolved.as_array().ok_or_else(|| Error::ParseError {
            offset: 0,
            reason: format!(
                "Type0 font '{}': DescendantFonts is not an array",
                base_font
            ),
        })?;

        if array.is_empty() {
            return Err(Error::ParseError {
                offset: 0,
                reason: format!(
                    "Type0 font '{}': DescendantFonts array is empty - must have at least 1 element",
                    base_font
                ),
            });
        }

        // Use first element (PDF spec: "Usually contains a single element")
        if array.len() > 1 {
            log::warn!(
                "Font '{}': DescendantFonts array has {} elements, using first",
                base_font,
                array.len()
            );
        }

        // accept both indirect
        // references AND direct dictionary objects in DescendantFonts.
        // PDF spec §9.7.6 mandates indirect refs, but Persian / Farsi
        // PDFs from older XeTeX / pdfTeX writers (Nazanin, Yagut,
        // Mitra, Lotus fonts) commonly inline the CIDFont dict
        // directly. Older versions rejected the inline form with
        // "DescendantFonts[0] is not a reference" and fell back to
        // Identity-H, which emits CIDs as Latin-Extended-B garbage
        // instead of mapping through the CIDSystemInfo collection.
        // Accepting the inline form gets the parser past this gate;
        // bundling the official Adobe-Persian-1-UCS2 /
        // Adobe-Arabic-1-UCS2 CMap data is a separate follow-up.
        let cidfont_obj_owned;
        let cidfont_dict = match array[0].as_reference() {
            Some(cidfont_ref) => {
                cidfont_obj_owned = doc.load_object(cidfont_ref)?;
                cidfont_obj_owned
                    .as_dict()
                    .ok_or_else(|| Error::ParseError {
                        offset: 0,
                        reason: format!("Type0 font '{}': CIDFont is not a dictionary", base_font),
                    })?
            }
            None => {
                // Inline-dict path — accept it per §9.7.6 lenient
                // reader posture.
                log::info!(
                    "Type0 font '{}': DescendantFonts[0] is a direct dictionary \
                     (non-conformant per §9.7.6 but recoverable); parsing inline",
                    base_font,
                );
                array[0].as_dict().ok_or_else(|| Error::ParseError {
                    offset: 0,
                    reason: format!(
                        "Type0 font '{}': DescendantFonts[0] is neither a reference \
                         nor a dictionary",
                        base_font
                    ),
                })?
            }
        };

        // Get CIDFont subtype (required: CIDFontType0 or CIDFontType2)
        let cid_font_type = cidfont_dict
            .get("Subtype")
            .and_then(|obj| obj.as_name())
            .ok_or_else(|| Error::ParseError {
                offset: 0,
                reason: format!(
                    "Type0 font '{}': CIDFont missing required /Subtype",
                    base_font
                ),
            })?
            .to_string();

        // Validate subtype
        if cid_font_type != "CIDFontType0" && cid_font_type != "CIDFontType2" {
            return Err(Error::ParseError {
                offset: 0,
                reason: format!(
                    "Type0 font '{}': Invalid CIDFontType '{}' (must be CIDFontType0 or CIDFontType2)",
                    base_font, cid_font_type
                ),
            });
        }

        // Parse CIDSystemInfo (required for all CIDFonts)
        let cid_system_info = match Self::parse_cidsysteminfo(cidfont_dict, doc) {
            Ok(info) => Some(info),
            Err(e) => {
                log::warn!(
                    "Font '{}': Failed to parse CIDSystemInfo: {}. Continuing with None.",
                    base_font,
                    e
                );
                None
            }
        };

        // Parse CIDToGIDMap (only for CIDFontType2 - TrueType-based)
        let cid_to_gid_map = if cid_font_type == "CIDFontType2" {
            match cidfont_dict.get("CIDToGIDMap") {
                None => {
                    // Default to Identity if not specified
                    log::debug!(
                        "Font '{}': CIDToGIDMap not specified, defaulting to Identity",
                        base_font
                    );
                    Some(CIDToGIDMap::Identity)
                }
                Some(cidtogid_obj) => {
                    // Handle Name object "/Identity"
                    if let Some(name) = cidtogid_obj.as_name() {
                        if name == "Identity" {
                            log::debug!("Font '{}': CIDToGIDMap is Identity", base_font);
                            Some(CIDToGIDMap::Identity)
                        } else {
                            log::warn!(
                                "Font '{}': Invalid CIDToGIDMap name '{}' (only 'Identity' is valid as name)",
                                base_font,
                                name
                            );
                            Some(CIDToGIDMap::Identity) // Fallback
                        }
                    } else if let Some(stream_ref) = cidtogid_obj.as_reference() {
                        // Handle Stream object (binary uint16 array)
                        match doc.load_object(stream_ref) {
                            Ok(stream_obj) => {
                                match doc.decode_stream_with_encryption(&stream_obj, stream_ref) {
                                    Ok(stream_data) => {
                                        // Validate stream length (must be even)
                                        if stream_data.len() % 2 != 0 {
                                            log::warn!(
                                            "Font '{}': CIDToGIDMap stream has odd length {} (must be even). Using Identity fallback.",
                                            base_font,
                                            stream_data.len()
                                        );
                                            Some(CIDToGIDMap::Identity)
                                        } else if stream_data.is_empty() {
                                            log::warn!(
                                            "Font '{}': CIDToGIDMap stream is empty. Using Identity fallback.",
                                            base_font
                                        );
                                            Some(CIDToGIDMap::Identity)
                                        } else {
                                            // Parse big-endian uint16 array
                                            let num_entries = stream_data.len() / 2;
                                            let mut map = Vec::with_capacity(num_entries);
                                            for i in 0..num_entries {
                                                let gid = u16::from_be_bytes([
                                                    stream_data[i * 2],
                                                    stream_data[i * 2 + 1],
                                                ]);
                                                map.push(gid);
                                            }
                                            log::debug!(
                                            "Font '{}': Loaded explicit CIDToGIDMap with {} entries",
                                            base_font,
                                            num_entries
                                        );
                                            Some(CIDToGIDMap::Explicit(map))
                                        }
                                    }
                                    Err(e) => {
                                        log::warn!(
                                        "Font '{}': CIDToGIDMap stream decode failed: {}. Using Identity fallback.",
                                        base_font,
                                        e
                                    );
                                        Some(CIDToGIDMap::Identity)
                                    }
                                }
                            }
                            Err(e) => {
                                log::warn!(
                                    "Font '{}': CIDToGIDMap stream object load failed: {}. Using Identity fallback.",
                                    base_font,
                                    e
                                );
                                Some(CIDToGIDMap::Identity)
                            }
                        }
                    } else {
                        log::warn!(
                            "Font '{}': CIDToGIDMap is neither Name nor Stream reference. Using Identity fallback.",
                            base_font
                        );
                        Some(CIDToGIDMap::Identity)
                    }
                }
            }
        } else {
            // CIDFontType0 (CFF/OpenType) doesn't use CIDToGIDMap
            log::debug!(
                "Font '{}': CIDFontType0 (CFF/OpenType) - no CIDToGIDMap needed",
                base_font
            );
            None
        };

        // Parse /DW (default width for CIDs) - PDF Spec Section 9.7.4.3
        // Default is 1000 if not specified
        let dw_value = cidfont_dict.get("DW").and_then(|obj| {
            // Resolve indirect reference if needed
            let resolved = if let Some(r) = obj.as_reference() {
                doc.load_object(r).ok()
            } else {
                Some(obj.clone())
            };
            resolved.and_then(|o| match &o {
                Object::Integer(i) => Some(*i as f32),
                Object::Real(r) => Some(*r as f32),
                _ => None,
            })
        });
        // F14/F15 fix: track whether /DW was explicitly present in the PDF.
        let has_explicit_dw = dw_value.is_some();
        let cid_default_width = dw_value.unwrap_or(1000.0);

        // Parse /W array (CID widths) - PDF Spec Section 9.7.4.3
        // Resolve /W reference if needed before parsing (common for large arrays)
        let resolved_cidfont_dict = if let Some(w_obj) = cidfont_dict.get("W") {
            if let Some(r) = w_obj.as_reference() {
                match doc.load_object(r) {
                    Ok(resolved) => {
                        let mut dict_clone = cidfont_dict.clone();
                        dict_clone.insert("W".to_string(), resolved);
                        std::borrow::Cow::Owned(dict_clone)
                    }
                    Err(e) => {
                        log::warn!(
                            "Font '{}': Failed to resolve /W reference: {}",
                            base_font,
                            e
                        );
                        std::borrow::Cow::Borrowed(cidfont_dict)
                    }
                }
            } else {
                std::borrow::Cow::Borrowed(cidfont_dict)
            }
        } else {
            std::borrow::Cow::Borrowed(cidfont_dict)
        };
        let cid_widths = Self::parse_cid_widths(&resolved_cidfont_dict, base_font);

        if cid_widths.is_some() {
            log::debug!(
                "Font '{}': Parsed CID widths - {} entries, default width {}",
                base_font,
                cid_widths.as_ref().map(|m| m.len()).unwrap_or(0),
                cid_default_width
            );
        }

        // Parse /W2 (per-CID vertical metrics) and /DW2 (default vertical
        // metrics) — ISO 32000-1 §9.7.4.3. Most fonts ship horizontal-only;
        // when /W2 is absent the per-CID HashMap is never allocated.
        let resolved_for_w2 = if let Some(w2_obj) = cidfont_dict.get("W2") {
            if let Some(r) = w2_obj.as_reference() {
                match doc.load_object(r) {
                    Ok(resolved) => {
                        let mut dict_clone = resolved_cidfont_dict.clone().into_owned();
                        dict_clone.insert("W2".to_string(), resolved);
                        std::borrow::Cow::Owned(dict_clone)
                    }
                    Err(e) => {
                        log::warn!(
                            "Font '{}': Failed to resolve /W2 reference: {}",
                            base_font,
                            e
                        );
                        resolved_cidfont_dict.clone()
                    }
                }
            } else {
                resolved_cidfont_dict.clone()
            }
        } else {
            resolved_cidfont_dict.clone()
        };
        let cid_vertical_metrics = Self::parse_cid_vertical_metrics(&resolved_for_w2, base_font);
        let cid_default_vertical_metrics = Self::parse_dw2(&resolved_for_w2);
        if cid_vertical_metrics.is_some() {
            log::debug!(
                "Font '{}': Parsed /W2 vertical metrics - {} entries, /DW2 defaults w1y={} v_x={} v_y={}",
                base_font,
                cid_vertical_metrics.as_ref().map(|m| m.len()).unwrap_or(0),
                cid_default_vertical_metrics.w1y,
                cid_default_vertical_metrics.v_x,
                cid_default_vertical_metrics.v_y,
            );
        }

        // Extract TrueType cmap from descendant's FontDescriptor if available.
        // Type0 parent fonts often have no embedded data — it's on the CIDFont.
        let descendant_tt_cmap = if cid_font_type == "CIDFontType2" {
            Self::extract_truetype_cmap_from_descriptor(cidfont_dict, base_font, doc)
        } else {
            None
        };

        // Extract embedded font data from CIDFont's FontDescriptor.
        // Per PDF spec, embedded font programs for Type0 fonts live on the
        // CIDFont descendant's FontDescriptor, not on the Type0 wrapper.
        let descendant_embedded =
            Self::extract_embedded_font_from_descriptor(cidfont_dict, base_font, doc);

        // Extract ascent/descent from the CIDFont's FontDescriptor (§9.7.4 / Table 117).
        // The Type0 wrapper has no top-level /FontDescriptor, so these values must be
        // read from the descendant.
        let (desc_raw_ascent, desc_raw_descent) =
            Self::read_raw_ascent_descent_from_descriptor(cidfont_dict, doc);

        Ok((
            cid_to_gid_map,
            cid_system_info,
            Some(cid_font_type),
            cid_widths,
            cid_default_width,
            has_explicit_dw,
            descendant_tt_cmap,
            descendant_embedded,
            desc_raw_ascent,
            desc_raw_descent,
            cid_vertical_metrics,
            cid_default_vertical_metrics,
        ))
    }
}
