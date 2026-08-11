use super::*;

impl FontInfo {
    /// Parse font information from a font dictionary object.
    ///
    /// # Arguments
    ///
    /// * `dict` - The font dictionary object (should be a Dictionary or Stream)
    /// * `doc` - The PDF document (needed to load referenced objects)
    ///
    /// # Returns
    ///
    /// A FontInfo struct containing the parsed font information.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The object is not a dictionary
    /// - Required font dictionary entries are missing or invalid
    /// - Referenced objects cannot be loaded
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use pdf_oxide::document::PdfDocument;
    /// use pdf_oxide::fonts::FontInfo;
    /// use pdf_oxide::object::ObjectRef;
    ///
    /// # fn example(doc: PdfDocument, font_ref: ObjectRef) -> Result<(), Box<dyn std::error::Error>> {
    /// let font_obj = doc.load_object(font_ref)?;
    /// let font_info = FontInfo::from_dict(&font_obj, &doc)?;
    /// println!("Font: {}", font_info.base_font);
    /// # Ok(())
    /// # }
    /// ```
    pub fn from_dict(dict: &Object, doc: &PdfDocument) -> Result<Self> {
        let font_dict = dict.as_dict().ok_or_else(|| Error::ParseError {
            offset: 0,
            reason: "Font object is not a dictionary".to_string(),
        })?;

        // Extract BaseFont (required)
        let base_font = font_dict
            .get("BaseFont")
            .and_then(|obj| obj.as_name())
            .unwrap_or("Unknown")
            .to_string();

        // Extract Subtype (required)
        let subtype = font_dict
            .get("Subtype")
            .and_then(|obj| obj.as_name())
            .unwrap_or("Unknown")
            .to_string();

        // Log Type 3 fonts - may require special glyph name mapping
        if subtype == "Type3" {
            let msg = format!(
                "Font '{}' is Type 3 - may require special glyph name mapping",
                base_font
            );
            log::warn!("{}", msg);
            // push into the structured warning
            // sink. PDF Spec §9.6.4 "Type 3 Fonts" describes the
            // user-defined CharProcs glyph-program model; the
            // standard glyph name registry doesn't apply, so
            // extraction may fall back to glyph-name heuristics.
            crate::extractors::warnings::push_global_warning(
                crate::extractors::warnings::Warning {
                    category: crate::extractors::warnings::WarningCategory::Type3Font,
                    page: None,
                    message: msg,
                    spec_section: Some("9.6.4"),
                },
            );
        }

        // Parse FontMatrix [a] for Type 3 fonts.
        // Standard Type 1 FontMatrix is [0.001 0 0 0.001 0 0], so widths are in 1/1000 em.
        // Type 3 fonts can use an identity FontMatrix [1 0 0 1 0 0], meaning widths are
        // in text-space units directly (no 1/1000 scaling needed).
        let font_matrix_a = if subtype == "Type3" {
            font_dict
                .get("FontMatrix")
                .and_then(|obj| obj.as_array())
                .and_then(|arr| arr.first())
                .and_then(|v| {
                    v.as_real()
                        .map(|r| r as f32)
                        .or_else(|| v.as_integer().map(|i| i as f32))
                })
                // A degenerate FontMatrix[0] — zero, near-zero, or non-finite —
                // is a malformed horizontal scale (ISO 32000-1 §9.2.4 / §9.6.5)
                // and would make the `default_width * 0.001 / font_matrix_a`
                // rescale below divide by ~0 → inf/NaN, and the
                // `font_size * font_matrix_a` advance collapse to 0. Reject it
                // and fall back to the standard 0.001 (Type 1) scale.
                .filter(|a| a.is_finite() && a.abs() > 1e-6)
                .unwrap_or(0.001)
        } else {
            0.001
        };

        let DescriptorData {
            font_weight,
            flags,
            stem_v,
            mut embedded_font_data,
            is_truetype_font,
            raw_ascent,
            raw_descent,
            mut has_font_program,
        } = parse_font_descriptor(font_dict, &base_font, doc);

        // TrueType cmap extraction is now LAZY — deferred until first access via
        // truetype_cmap() accessor. This saves 10-25ms per font when ToUnicode CMap
        // (Priority 1) resolves all characters, making the cmap unnecessary.
        // The is_truetype_font flag is recorded here for the lazy accessor to use.

        // Helper function to check if font is symbolic (bit 3 set)
        let is_symbolic_font = |flags_opt: Option<i32>| -> bool {
            if let Some(flags_value) = flags_opt {
                const SYMBOLIC_BIT: i32 = 1 << 2; // Bit 3
                (flags_value & SYMBOLIC_BIT) != 0
            } else {
                // Fallback: check font name
                let name_lower = base_font.to_lowercase();
                name_lower.contains("symbol")
                    || name_lower.contains("zapf")
                    || name_lower.contains("dingbat")
            }
        };

        // Parse encoding (now that we have flags)
        // PDF Spec: ISO 32000-1:2008, Section 9.6.6.1
        // "For symbolic fonts, the Encoding entry is ignored"
        //
        // However, many PDF generators (LaTeX, LibreOffice, etc.) incorrectly set the
        // Symbolic flag on non-symbolic fonts. When an explicit /Encoding entry exists,
        // we always parse it — real-world PDF viewers (MuPDF, poppler, pdf.js) do the same.
        // The Symbolic flag only controls behavior when NO /Encoding is present.
        // Pre-parse font program encoding (needed for /Differences base encoding per PDF spec)
        let font_program_enc_cache: Option<HashMap<u8, char>> =
            if let Some(font_data) = &embedded_font_data {
                if subtype == "Type1" || subtype == "MMType1" {
                    crate::fonts::type1_encoding::parse_type1_encoding(font_data)
                } else {
                    crate::fonts::cff_encoding::parse_cff_encoding(font_data)
                }
            } else {
                None
            };

        // Writing-mode signal sourced from the encoding object. Resolved
        // here because the `Encoding` enum collapses `Identity-H` and
        // `Identity-V` to the same `Encoding::Identity` variant — we need
        // the original name to recover wmode. Defaults to `0` (horizontal)
        // when no encoding object is present.
        let mut encoding_wmode: u8 = 0;
        let (encoding, diff_multi_char_map, diff_glyph_names) = if let Some(enc_obj) =
            font_dict.get("Encoding")
        {
            let resolved_enc_obj = if let Some(obj_ref) = enc_obj.as_reference() {
                doc.load_object(obj_ref)?
            } else {
                enc_obj.clone()
            };

            // Inspect for `-V` predefined name or embedded `/WMode 1 def`
            // before parse_encoding flattens the variant.
            let (_enc_name, wm) = Self::resolve_encoding_writing_mode(&resolved_enc_obj, doc);
            encoding_wmode = wm;

            if is_symbolic_font(flags) {
                log::debug!(
                    "Font '{}' is symbolic (Flags={:?}) but has /Encoding — parsing it anyway (common in LaTeX/LibreOffice PDFs)",
                    base_font,
                    flags
                );
            } else {
                log::debug!("Font '{}' using /Encoding entry", base_font);
            }
            let (mut parsed_enc, mut multi_map, glyph_names) =
                Self::parse_encoding(&resolved_enc_obj, doc, font_program_enc_cache.as_ref())?;

            // When /Encoding is a named encoding (e.g., /WinAnsiEncoding) AND the font
            // has an embedded program, merge the font program's encoding. This handles
            // fonts where the program maps glyphs to non-standard code positions
            // (e.g., space at 0xCA) that the named encoding maps differently.
            // The font program's mappings override the standard encoding.
            if matches!(parsed_enc, Encoding::Standard(_)) {
                if let Some(prog_enc) = &font_program_enc_cache {
                    let std_name = match &parsed_enc {
                        Encoding::Standard(n) => n.clone(),
                        _ => "StandardEncoding".to_string(),
                    };

                    // Decide whether the embedded program's built-in encoding is a
                    // meaningful text encoding (a few non-standard slots to overlay,
                    // e.g. space at 0xCA) or a re-indexed *cipher* — a subset font's
                    // own glyph ordering that bears no relation to the producer's
                    // declared named base encoding. Overlaying a cipher rewrites every
                    // mapped code into mojibake. Discriminate by agreement: count how
                    // many program codes resolve to the SAME character the named base
                    // already gives. A real encoding agrees on most; a cipher on
                    // almost none.
                    let looks_like_cipher = builtin_encoding_looks_like_cipher(prog_enc, &std_name);

                    if looks_like_cipher {
                        // Trust the producer-declared named encoding; the built-in
                        // cipher would corrupt it. Leave `parsed_enc` as the named
                        // Standard encoding.
                        log::debug!(
                            "Font '{base_font}': built-in encoding disagrees with {std_name} on most overlapping codes — treating as a subset cipher and keeping the named encoding"
                        );
                    } else {
                        log::info!(
                            "Font '{}': merging {} font program encoding entries with {}",
                            base_font,
                            prog_enc.len(),
                            std_name,
                        );
                        // Build Custom map: start with the named encoding, overlay the
                        // (consistent) font program for its few non-standard slots.
                        let mut custom_map: HashMap<u8, char> = HashMap::new();
                        for code in 0u8..=255 {
                            if let Some(unicode_str) = standard_encoding_lookup(&std_name, code) {
                                if let Some(ch) = unicode_str.chars().next() {
                                    custom_map.insert(code, ch);
                                }
                            }
                        }
                        for (&code, &ch) in prog_enc {
                            custom_map.insert(code, ch);
                            if is_ligature_char(ch) {
                                if let Some(expanded) = expand_ligature_char(ch) {
                                    multi_map.insert(code, expanded.to_string());
                                }
                            }
                        }
                        parsed_enc = Encoding::Custom(custom_map);
                    }
                }
            }

            (parsed_enc, multi_map, glyph_names)
        } else {
            // No /Encoding entry — use font program's built-in encoding if available
            if let Some(prog_enc) = font_program_enc_cache {
                log::info!(
                    "Font '{}' using built-in font program encoding ({} mappings)",
                    base_font,
                    prog_enc.len()
                );
                let mut multi_map: HashMap<u8, String> = HashMap::new();
                for (&code, &ch) in &prog_enc {
                    if is_ligature_char(ch) {
                        if let Some(expanded) = expand_ligature_char(ch) {
                            multi_map.insert(code, expanded.to_string());
                        }
                    }
                }
                (Encoding::Custom(prog_enc), multi_map, HashMap::new())
            } else if is_symbolic_font(flags) {
                log::debug!(
                    "Font '{}' is symbolic with no /Encoding - will use built-in encoding (Symbol/ZapfDingbats)",
                    base_font
                );
                (
                    Encoding::Standard("SymbolicBuiltIn".to_string()),
                    HashMap::new(),
                    HashMap::new(),
                )
            } else {
                log::debug!(
                    "Font '{}' has no /Encoding entry - defaulting to StandardEncoding",
                    base_font
                );
                (
                    Encoding::Standard("StandardEncoding".to_string()),
                    HashMap::new(),
                    HashMap::new(),
                )
            }
        };

        // Parse ToUnicode CMap if present (Phase 5.1: Lazy Loading)
        // The CMap stream is stored raw and parsed only on first character lookup
        let to_unicode = if let Some(cmap_ref) = font_dict
            .get("ToUnicode")
            .and_then(|obj| obj.as_reference())
        {
            let stream_opt = match doc.load_object(cmap_ref) {
                Ok(cmap_obj) => {
                    match doc.decode_stream_with_encryption(&cmap_obj, cmap_ref) {
                        Ok(data) => Some(data),
                        Err(e) => {
                            log::warn!(
                                "Font '{}': Failed to decrypt/decode ToUnicode CMap stream {:?}: {}",
                                base_font, cmap_ref, e
                            );
                            None
                        }
                    }
                }
                Err(e) => {
                    log::warn!(
                        "Font '{}': Failed to load ToUnicode CMap object {:?}: {}",
                        base_font,
                        cmap_ref,
                        e
                    );
                    None
                }
            };

            if let Some(stream_bytes) = stream_opt {
                // Store raw bytes for lazy parsing — LazyCMap handles errors on first access.
                // Skipping eager validation avoids parsing every CMap twice.
                log::info!(
                    "ToUnicode CMap stream loaded for font '{}': {} bytes (lazy parsing enabled)",
                    base_font,
                    stream_bytes.len()
                );
                Some(LazyCMap::new(stream_bytes))
            } else {
                // Specific error already logged above in the match arms
                None
            }
        } else {
            if subtype == "Type0" {
                let msg = format!("Type0 font '{}' has no ToUnicode entry!", base_font);
                log::warn!("{}", msg);
                // push to the structured sink. PDF
                // Spec §9.10.2 "ToUnicode CMaps" describes the
                // mapping; absent ToUnicode triggers the fallback
                // chain (Encoding → AGL → CID-as-Unicode) per §9.10.3.
                crate::extractors::warnings::push_global_warning(
                    crate::extractors::warnings::Warning {
                        category: crate::extractors::warnings::WarningCategory::ToUnicodeMissing,
                        page: None,
                        message: msg,
                        spec_section: Some("9.10.2"),
                    },
                );
            }
            None
        };

        // Parse /Widths array for glyph width information
        // PDF Spec: ISO 32000-1:2008, Section 9.7.4 - Font Widths
        //
        // For simple fonts (Type1, TrueType), widths are specified as an array
        // of integers in 1000ths of em, indexed from FirstChar to LastChar.
        //
        // Note: Type0 (CID) fonts use a different /W array format, parsed via parse_descendant_fonts below
        let (widths, first_char, last_char) = if subtype != "Type0" {
            // Try to parse /Widths array
            let widths_opt = font_dict.get("Widths").and_then(|widths_obj| {
                // Handle both direct arrays and references
                let resolved = if let Some(ref_obj) = widths_obj.as_reference() {
                    doc.load_object(ref_obj).ok()?
                } else {
                    widths_obj.clone()
                };

                resolved.as_array().map(|arr| {
                    arr.iter()
                        .filter_map(|obj| {
                            // Widths can be integers or reals
                            obj.as_integer()
                                .map(|i| i as f32)
                                .or_else(|| obj.as_real().map(|r| r as f32))
                        })
                        .collect::<Vec<f32>>()
                })
            });

            let first = font_dict
                .get("FirstChar")
                .and_then(|obj| obj.as_integer())
                .map(|i| i as u32);

            let last = font_dict
                .get("LastChar")
                .and_then(|obj| obj.as_integer())
                .map(|i| i as u32);

            if widths_opt.is_some() {
                log::debug!(
                    "Font '{}': parsed {} widths (FirstChar={:?}, LastChar={:?})",
                    base_font,
                    widths_opt.as_ref().map(|w| w.len()).unwrap_or(0),
                    first,
                    last
                );
            } else {
                log::debug!(
                    "Font '{}': no /Widths array found, will use default width",
                    base_font
                );
            }

            (widths_opt, first, last)
        } else {
            // Type0 fonts use /W and /DW arrays parsed via parse_descendant_fonts
            log::debug!(
                "Font '{}': Type0 font, widths parsed from CIDFont /W array",
                base_font
            );
            (None, None, None)
        };

        // Set default width based on font characteristics
        // PDF Spec: Typical values are 500-600 for proportional fonts, ~600 for monospace
        let default_width = if let Some(flags_val) = flags {
            const FIXED_PITCH_BIT: i32 = 1 << 0; // Bit 1
            if (flags_val & FIXED_PITCH_BIT) != 0 {
                600.0 // Monospace font
            } else {
                500.0 // Proportional font
            }
        } else {
            // No flags, use middle-ground default
            550.0
        };

        // The heuristic above is calibrated for standard fonts where font_matrix_a = 0.001
        // (i.e. glyph-space units are 1/1000 em).  Type3 fonts can use an arbitrary
        // FontMatrix; if font_matrix_a differs from 0.001, rescale so that callers
        // multiplying by font_matrix_a still get the intended em-fraction result.
        let default_width = if subtype == "Type3" && font_matrix_a != 0.001 {
            default_width * 0.001 / font_matrix_a
        } else {
            default_width
        };

        // Phase 3: Parse DescendantFonts for Type0 fonts
        let (
            cid_to_gid_map,
            cid_system_info,
            cid_font_type,
            cid_widths,
            cid_default_width,
            has_explicit_dw,
            descendant_tt_cmap,
            desc_raw_ascent,
            desc_raw_descent,
            cid_vertical_metrics,
            cid_default_vertical_metrics,
        ) = if subtype == "Type0" {
            match Self::parse_descendant_fonts(font_dict, &base_font, doc) {
                Ok((
                    map,
                    info,
                    ftype,
                    widths,
                    dw,
                    explicit_dw,
                    tt_cmap,
                    (desc_has_font_program, desc_embedded),
                    d_ascent,
                    d_descent,
                    vmetrics,
                    dvmetrics,
                )) => {
                    log::info!(
                            "Font '{}': Parsed DescendantFonts - CIDFontType={}, CIDSystemInfo={}-{}, widths={}, embedded={}",
                            base_font,
                            ftype.as_ref().unwrap_or(&"Unknown".to_string()),
                            info.as_ref()
                                .map(|s| s.registry.as_str())
                                .unwrap_or("Unknown"),
                            info.as_ref()
                                .map(|s| s.ordering.as_str())
                                .unwrap_or("Unknown"),
                            widths.as_ref().map(|m| m.len()).unwrap_or(0),
                            desc_embedded.is_some()
                        );
                    // Use embedded font data from CIDFont descendant if top-level didn't have it
                    if desc_embedded.is_some() && embedded_font_data.is_none() {
                        embedded_font_data = desc_embedded;
                    }
                    has_font_program |= desc_has_font_program;
                    (
                        map,
                        info,
                        ftype,
                        widths,
                        dw,
                        explicit_dw,
                        tt_cmap,
                        d_ascent,
                        d_descent,
                        vmetrics,
                        dvmetrics,
                    )
                }
                Err(e) => {
                    log::warn!(
                        "Font '{}': Failed to parse DescendantFonts: {}. Using Identity fallback.",
                        base_font,
                        e
                    );
                    (
                        Some(CIDToGIDMap::Identity),
                        None,
                        None,
                        None,
                        1000.0,
                        false,
                        None,
                        None,
                        None,
                        None,
                        VerticalMetrics::SPEC_DEFAULT,
                    )
                }
            }
        } else {
            (
                None,
                None,
                None,
                None,
                1000.0,
                false,
                None,
                None,
                None,
                None,
                VerticalMetrics::SPEC_DEFAULT,
            )
        };

        // For Type0 fonts the /FontDescriptor lives on the CIDFont descendant (§9.7.4).
        // If the top-level font had no descriptor (the common case), fall back to the
        // descendant's values so CID/CJK glyphs get real metrics instead of the 0.95/-0.35
        // Poppler-compatible default.
        let raw_ascent = raw_ascent.or(desc_raw_ascent);
        let raw_descent = raw_descent.or(desc_raw_descent);

        // Pre-populate OnceLock with descendant's TrueType cmap if available.
        // Otherwise leave it for lazy extraction from embedded_font_data.
        let truetype_cmap_lock = std::sync::OnceLock::new();
        if let Some(desc_cmap) = descendant_tt_cmap {
            let _ = truetype_cmap_lock.set(Some(desc_cmap));
        }

        // Parse CFF GID mapping ONLY for simple (non-Type0) fonts with embedded CFF data.
        // Type0/CID fonts use Identity-H encoding and CIDToGIDMap, not CFF Standard Encoding.
        //
        // §9.6.6: the byte → GID resolution must use the PDF font dictionary's
        // /Encoding as the byte → glyph-name source and the CFF Charset as the
        // glyph-name → GID resolver. Subsetter-emitted custom CFF Encoding
        // tables are frequently sparse (some prepress subsetters emit only
        // `space` and `A`) and would silently drop most content bytes to
        // `.notdef` without this routing.
        let cff_gid_map = if subtype != "Type0" {
            embedded_font_data.as_ref().and_then(|data| {
                crate::fonts::cff_encoding::parse_cff_gid_mapping_with_pdf_encoding(
                    data,
                    &encoding,
                    &diff_glyph_names,
                )
                .inspect(|map| {
                    log::debug!(
                        "Font '{}': parsed CFF GID mapping via PDF /Encoding ({} entries)",
                        base_font,
                        map.len()
                    );
                })
            })
        } else {
            None
        };

        // Normalize ascent/descent from 1000ths-of-em to fraction-of-em.
        // PDF spec says these are in 1/1000 of em (glyph space units).
        // Fall back to standard font metrics for the 14 standard PDF fonts,
        // then to Poppler-compatible defaults (0.95 / -0.35).
        let (default_ascent, default_descent) =
            standard_font_metrics(&base_font).unwrap_or((0.95, -0.35));
        let ascent = raw_ascent.map(|v| v / 1000.0).unwrap_or(default_ascent);
        // PDF Descent should be ≤ 0 (below baseline). Some PDFs store it as a positive
        // magnitude; Poppler normalizes by negating. Mirror that here.
        let descent = raw_descent
            .map(|v| {
                let d = v / 1000.0;
                if d > 0.0 {
                    -d
                } else {
                    d
                }
            })
            .unwrap_or(default_descent);

        // Final writing-mode resolution.
        //
        // Per ISO 32000-1:2008 §9.10.2 the ToUnicode CMap is for
        // extraction-time character → Unicode mapping ONLY. The active
        // writing mode is determined by the /Encoding CMap (§9.7.5):
        // either an embedded `/WMode 1 def` directive or a predefined
        // encoding name whose suffix is `-V`. Consulting the ToUnicode
        // CMap's `/WMode` here would silently flip a horizontal document
        // to vertical whenever a producer left a stale `/WMode 1 def`
        // in the ToUnicode prologue — a real-world tooling failure mode.
        //
        // We still emit a debug log when ToUnicode disagrees with the
        // /Encoding so producer bugs are diagnosable.
        let wmode = encoding_wmode;
        if let Some(tu) = to_unicode.as_ref() {
            let tu_wmode = tu.wmode();
            if tu_wmode != encoding_wmode {
                log::debug!(
                    "Font '{}': ToUnicode CMap declares /WMode {} but /Encoding wmode is {}. \
                     Honoring /Encoding per ISO 32000-1 §9.10.2.",
                    base_font,
                    tu_wmode,
                    encoding_wmode
                );
            }
        }

        // Detect Adobe predefined CIDFont substitution candidates.
        // Conditions (all must hold):
        //   1. Type0 font (the only place predefined CMaps are referenced).
        //   2. No embedded font program — neither the Type0 wrapper's nor the
        //      CIDFont descendant's FontDescriptor carries a `/FontFile{,2,3}`
        //      KEY. Key presence (not extraction success) is what gates here:
        //      a present-but-undecodable program means the document embeds its
        //      own outlines and the decode failure must surface as a warning,
        //      not be masked by a silent sans-serif substitution.
        //   3. The /Encoding resolves to an Identity charcode→CID mapping
        //      (Identity-H/V or an Adobe-collection identity CMap stream).
        //      Non-Identity predefined CMaps (90ms-RKSJ-H, GBK-EUC-H, …) carry
        //      raw legacy multi-byte codes, not CIDs — substituting would
        //      index the CID→Unicode tables with Shift-JIS / EUC values and
        //      paint wrong glyphs. Those CMaps stay unsubstituted until a
        //      charcode→CID CMap pass is wired.
        //   4. The base font name (after subset-prefix + CMap-suffix strip)
        //      matches one of the registered predefined names from
        //      Technical Notes #5078 / #5079 / #5080 / #5093.
        // The character collection comes from the descendant's /CIDSystemInfo
        // Ordering when it names a known collection (it is authoritative for
        // CID semantics per ISO 32000-1 §9.7.3); the name-derived collection
        // is the fallback for Identity/unknown orderings.
        // When all hold, the renderer routes the paint through the bundled
        // covering font; otherwise we leave `cjk_substitution` at `None` and
        // the existing render path runs unchanged.
        let cjk_substitution = if subtype == "Type0"
            && !has_font_program
            && embedded_font_data.is_none()
            && matches!(encoding, Encoding::Identity)
        {
            use crate::fonts::predefined_cidfont::CharacterCollection;
            let name_collection = crate::fonts::predefined_cidfont::is_predefined(&base_font);
            let ordering_collection =
                cid_system_info
                    .as_ref()
                    .and_then(|info| match info.ordering.as_str() {
                        "Japan1" => Some(CharacterCollection::AdobeJapan1),
                        "GB1" => Some(CharacterCollection::AdobeGB1),
                        "CNS1" => Some(CharacterCollection::AdobeCNS1),
                        "Korea1" => Some(CharacterCollection::AdobeKorea1),
                        _ => None,
                    });
            let collection = match (name_collection, ordering_collection) {
                (Some(n), Some(o)) if n != o => {
                    log::info!(
                        "Font '{}': base name implies collection {:?} but \
                         /CIDSystemInfo Ordering says {:?}; trusting CIDSystemInfo",
                        base_font,
                        n,
                        o
                    );
                    Some(o)
                }
                (Some(n), _) => Some(n),
                (None, _) => None,
            };
            if let Some(c) = collection {
                log::info!(
                    "Font '{}': flagged for CJK predefined-CIDFont substitution \
                     (collection {:?}); no embedded outlines, base name is an \
                     Adobe predefined CIDFont per ISO 32000-2 §9.7.5.2",
                    base_font,
                    c
                );
            }
            collection
        } else {
            if subtype == "Type0"
                && crate::fonts::predefined_cidfont::is_predefined(&base_font).is_some()
            {
                if has_font_program && embedded_font_data.is_none() {
                    log::warn!(
                        "Font '{}': /FontFile{{,2,3}} present but the font program \
                         failed to load/decode (see warnings above); NOT substituting \
                         the bundled CJK fallback — glyphs for this font will not render",
                        base_font
                    );
                } else if !has_font_program && !matches!(encoding, Encoding::Identity) {
                    log::info!(
                        "Font '{}': Adobe predefined CIDFont without embedded outlines, \
                         but /Encoding is a non-Identity CMap — charcodes are not CIDs, \
                         so CJK substitution is skipped",
                        base_font
                    );
                }
            }
            None
        };

        Ok(FontInfo {
            base_font,
            subtype,
            encoding,
            to_unicode,
            font_weight,
            flags,
            stem_v,
            ascent,
            descent,
            embedded_font_data,
            truetype_cmap: truetype_cmap_lock,
            embedded_glyph_names: std::sync::OnceLock::new(),
            is_truetype_font,
            cid_to_gid_map,
            cid_system_info,
            cid_font_type,
            font_matrix_a,
            widths,
            first_char,
            last_char,
            default_width,
            cid_widths,
            cid_default_width,
            has_explicit_dw,
            cff_gid_map,
            multi_char_map: diff_multi_char_map,
            byte_to_char_table: std::sync::OnceLock::new(),
            type0_unicode_memo: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            byte_to_width_table: std::sync::OnceLock::new(),
            weight_memo: std::sync::OnceLock::new(),
            italic_memo: std::sync::OnceLock::new(),
            std14_memo: std::sync::OnceLock::new(),
            diff_glyph_names,
            wmode,
            cid_vertical_metrics,
            cid_default_vertical_metrics,
            cjk_substitution,
        })
    }
}
