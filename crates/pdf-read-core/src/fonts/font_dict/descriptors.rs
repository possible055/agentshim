use super::*;

pub(super) struct DescriptorData {
    pub(super) font_weight: Option<i32>,
    pub(super) flags: Option<i32>,
    pub(super) stem_v: Option<f32>,
    pub(super) embedded_font_data: Option<Arc<Vec<u8>>>,
    pub(super) is_truetype_font: bool,
    pub(super) raw_ascent: Option<f32>,
    pub(super) raw_descent: Option<f32>,
    pub(super) has_font_program: bool,
}

pub(super) fn parse_font_descriptor(
    font_dict: &HashMap<String, Object>,
    base_font: &str,
    doc: &PdfDocument,
) -> DescriptorData {
    let (
        font_weight,
        flags,
        stem_v,
        embedded_font_data,
        is_truetype_font,
        raw_ascent,
        raw_descent,
        has_font_program,
    ) = if let Some(descriptor_ref) = font_dict
        .get("FontDescriptor")
        .and_then(|obj| obj.as_reference())
    {
        // Load the FontDescriptor object
        if let Ok(descriptor_obj) = doc.load_object(descriptor_ref) {
            if let Some(descriptor_dict) = descriptor_obj.as_dict() {
                let weight = descriptor_dict
                    .get("FontWeight")
                    .and_then(|weight_obj| weight_obj.as_integer())
                    .map(|w| w as i32);

                let descriptor_flags = descriptor_dict
                    .get("Flags")
                    .and_then(|flags_obj| flags_obj.as_integer())
                    .map(|f| f as i32);

                let stem_v_value = descriptor_dict.get("StemV").and_then(|sv_obj| {
                    sv_obj
                        .as_real()
                        .map(|r| r as f32)
                        .or_else(|| sv_obj.as_integer().map(|i| i as f32))
                });

                let ascent_value = descriptor_dict.get("Ascent").and_then(|obj| {
                    obj.as_real()
                        .map(|r| r as f32)
                        .or_else(|| obj.as_integer().map(|i| i as f32))
                });

                let descent_value = descriptor_dict.get("Descent").and_then(|obj| {
                    obj.as_real()
                        .map(|r| r as f32)
                        .or_else(|| obj.as_integer().map(|i| i as f32))
                });

                // Load embedded font data from FontFile2 (TrueType), FontFile (Type 1), or FontFile3 (CFF/OpenType)
                // IMPORTANT: Track whether font is TrueType or CFF - only TrueType fonts have cmaps!
                //
                // Key presence is recorded separately from extraction
                // success: a present-but-undecodable font program means
                // the document intended to be self-contained, which
                // downstream gates (CJK predefined-CIDFont substitution)
                // must distinguish from "no program at all".
                let has_font_program = descriptor_dict.contains_key("FontFile2")
                    || descriptor_dict.contains_key("FontFile3")
                    || descriptor_dict.contains_key("FontFile");
                let (embedded_font, is_truetype_font) =
                    if let Some(ff2_obj) = descriptor_dict.get("FontFile2") {
                        log::info!("Font '{}' has FontFile2 entry (TrueType)", base_font);
                        let font_data = ff2_obj
                            .as_reference()
                            .and_then(|ff2_ref| {
                                doc.load_object(ff2_ref)
                                    .inspect_err(|e| {
                                        log::warn!(
                                            "Font '{}': failed to load FontFile2 object {}: {}",
                                            base_font,
                                            ff2_ref,
                                            e
                                        )
                                    })
                                    .ok()
                                    .map(|obj| (obj, ff2_ref))
                            })
                            .and_then(|(ff2_stream, ff2_ref)| {
                                doc.decode_stream_with_encryption(&ff2_stream, ff2_ref)
                                    .inspect_err(|e| {
                                        log::warn!(
                                            "Font '{}': failed to decode FontFile2 stream {}: {}",
                                            base_font,
                                            ff2_ref,
                                            e
                                        )
                                    })
                                    .ok()
                            })
                            .map(|data| {
                                log::info!(
                                    "Font '{}' loaded embedded TrueType font ({} bytes)",
                                    base_font,
                                    data.len()
                                );
                                Arc::new(data)
                            });
                        (font_data, true) // TrueType - can have cmaps
                    } else if let Some(ff3_obj) = descriptor_dict.get("FontFile3") {
                        log::info!(
                            "Font '{}' has FontFile3 entry (CFF/OpenType - no TrueType cmap)",
                            base_font
                        );
                        let font_data = ff3_obj
                            .as_reference()
                            .and_then(|ff3_ref| {
                                doc.load_object(ff3_ref)
                                    .inspect_err(|e| {
                                        log::warn!(
                                            "Font '{}': failed to load FontFile3 object {}: {}",
                                            base_font,
                                            ff3_ref,
                                            e
                                        )
                                    })
                                    .ok()
                                    .map(|obj| (obj, ff3_ref))
                            })
                            .and_then(|(ff3_stream, ff3_ref)| {
                                doc.decode_stream_with_encryption(&ff3_stream, ff3_ref)
                                    .inspect_err(|e| {
                                        log::warn!(
                                            "Font '{}': failed to decode FontFile3 stream {}: {}",
                                            base_font,
                                            ff3_ref,
                                            e
                                        )
                                    })
                                    .ok()
                            })
                            .map(|data| {
                                // Wrap raw CFF in OpenType container for ttf-parser
                                let data = if !data.is_empty() && data[0] == 1 && data.len() > 4 {
                                    log::info!(
                                        "Font '{}': Wrapping raw CFF in OpenType ({} bytes)",
                                        base_font,
                                        data.len()
                                    );
                                    wrap_cff_in_opentype(&data)
                                } else {
                                    log::info!(
                                        "Font '{}' loaded embedded CFF/OpenType font ({} bytes)",
                                        base_font,
                                        data.len()
                                    );
                                    data
                                };
                                Arc::new(data)
                            });
                        (font_data, false) // CFF - no TrueType cmap
                    } else if let Some(ff_obj) = descriptor_dict.get("FontFile") {
                        log::info!("Font '{}' has FontFile entry (Type 1)", base_font);
                        let font_data = ff_obj
                            .as_reference()
                            .and_then(|ff_ref| {
                                doc.load_object(ff_ref)
                                    .inspect_err(|e| {
                                        log::warn!(
                                            "Font '{}': failed to load FontFile object {}: {}",
                                            base_font,
                                            ff_ref,
                                            e
                                        )
                                    })
                                    .ok()
                                    .map(|obj| (obj, ff_ref))
                            })
                            .and_then(|(ff_stream, ff_ref)| {
                                doc.decode_stream_with_encryption(&ff_stream, ff_ref)
                                    .inspect_err(|e| {
                                        log::warn!(
                                            "Font '{}': failed to decode FontFile stream {}: {}",
                                            base_font,
                                            ff_ref,
                                            e
                                        )
                                    })
                                    .ok()
                            })
                            .map(|data| {
                                log::info!(
                                    "Font '{}' loaded embedded Type 1 font ({} bytes)",
                                    base_font,
                                    data.len()
                                );
                                Arc::new(data)
                            });
                        (font_data, false) // Type 1 - no TrueType cmap
                    } else {
                        log::debug!("Font '{}' has no embedded font data", base_font);
                        (None, false)
                    };

                (
                    weight,
                    descriptor_flags,
                    stem_v_value,
                    embedded_font,
                    is_truetype_font,
                    ascent_value,
                    descent_value,
                    has_font_program,
                )
            } else {
                (None, None, None, None, false, None, None, false)
            }
        } else {
            (None, None, None, None, false, None, None, false)
        }
    } else {
        (None, None, None, None, false, None, None, false)
    };

    DescriptorData {
        font_weight,
        flags,
        stem_v,
        embedded_font_data,
        is_truetype_font,
        raw_ascent,
        raw_descent,
        has_font_program,
    }
}

impl FontInfo {
    /// Extract TrueType cmap from a font dictionary's /FontDescriptor /FontFile2.
    pub(super) fn extract_truetype_cmap_from_descriptor(
        font_dict: &HashMap<String, Object>,
        base_font: &str,
        doc: &PdfDocument,
    ) -> Option<TrueTypeCMap> {
        let desc_obj = font_dict.get("FontDescriptor")?;
        let desc = if let Some(r) = desc_obj.as_reference() {
            doc.load_object(r).ok()?
        } else {
            desc_obj.clone()
        };
        let desc_dict = desc.as_dict()?;
        let ff2_obj = desc_dict.get("FontFile2")?;
        let ff2_ref = ff2_obj.as_reference()?;
        let ff2_stream = match doc.load_object(ff2_ref) {
            Ok(obj) => obj,
            Err(e) => {
                log::warn!(
                    "Font '{}': Failed to load FontFile2 object {:?}: {}",
                    base_font,
                    ff2_ref,
                    e
                );
                return None;
            }
        };
        let font_data = match doc.decode_stream_with_encryption(&ff2_stream, ff2_ref) {
            Ok(data) => data,
            Err(e) => {
                log::warn!(
                    "Font '{}': Failed to decrypt/decode FontFile2 stream {:?}: {}",
                    base_font,
                    ff2_ref,
                    e
                );
                return None;
            }
        };
        if font_data.is_empty() {
            return None;
        }
        match TrueTypeCMap::from_font_data(&font_data) {
            Ok(cmap) if !cmap.is_empty() => {
                log::info!(
                    "Font '{}': Extracted TrueType cmap from descendant CIDFont ({} mappings)",
                    base_font,
                    cmap.len()
                );
                Some(cmap)
            }
            _ => None,
        }
    }

    /// Read raw /Ascent and /Descent from a font dictionary's /FontDescriptor.
    /// Returns (raw_ascent, raw_descent) in PDF 1/1000-em units, or None if absent.
    /// Used to pull ascent/descent off a CIDFont descendant (§9.7.4 / Table 117).
    pub(super) fn read_raw_ascent_descent_from_descriptor(
        font_dict: &HashMap<String, Object>,
        doc: &PdfDocument,
    ) -> (Option<f32>, Option<f32>) {
        let desc_obj = match font_dict.get("FontDescriptor") {
            Some(obj) => obj,
            None => return (None, None),
        };
        let desc = if let Some(r) = desc_obj.as_reference() {
            match doc.load_object(r) {
                Ok(obj) => obj,
                Err(_) => return (None, None),
            }
        } else {
            desc_obj.clone()
        };
        let desc_dict = match desc.as_dict() {
            Some(d) => d,
            None => return (None, None),
        };
        let read_f32 = |key: &str| -> Option<f32> {
            desc_dict.get(key).and_then(|o| {
                o.as_real()
                    .map(|r| r as f32)
                    .or_else(|| o.as_integer().map(|i| i as f32))
            })
        };
        (read_f32("Ascent"), read_f32("Descent"))
    }

    /// Extract embedded font data from a font dictionary's /FontDescriptor.
    /// Checks FontFile2 (TrueType), FontFile3 (CFF/OpenType), and FontFile (Type 1).
    pub(super) fn extract_embedded_font_from_descriptor(
        font_dict: &HashMap<String, Object>,
        base_font: &str,
        doc: &PdfDocument,
    ) -> (bool, Option<Arc<Vec<u8>>>) {
        let Some(desc_obj) = font_dict.get("FontDescriptor") else {
            return (false, None);
        };
        let desc = if let Some(r) = desc_obj.as_reference() {
            match doc.load_object(r) {
                Ok(obj) => obj,
                Err(_) => return (false, None),
            }
        } else {
            desc_obj.clone()
        };
        let Some(desc_dict) = desc.as_dict() else {
            return (false, None);
        };

        // Try FontFile2 (TrueType), FontFile3 (CFF/OpenType), FontFile (Type 1)
        let font_file_keys = ["FontFile2", "FontFile3", "FontFile"];
        // Key presence ≠ extraction success: callers gating on "the document
        // embeds its own outlines" must see `true` even when every present
        // stream fails to load/decode below.
        let has_font_program = font_file_keys
            .iter()
            .any(|key| desc_dict.contains_key(*key));
        for key in &font_file_keys {
            if let Some(ff_obj) = desc_dict.get(*key) {
                let ff_ref = match ff_obj.as_reference() {
                    Some(r) => r,
                    None => continue,
                };
                let ff_stream = match doc.load_object(ff_ref) {
                    Ok(obj) => obj,
                    Err(e) => {
                        log::warn!(
                            "Font '{}': Failed to load {} {:?}: {}",
                            base_font,
                            key,
                            ff_ref,
                            e
                        );
                        continue;
                    }
                };
                let font_data = match doc.decode_stream_with_encryption(&ff_stream, ff_ref) {
                    Ok(data) => data,
                    Err(e) => {
                        log::warn!(
                            "Font '{}': Failed to decode {} stream: {}",
                            base_font,
                            key,
                            e
                        );
                        continue;
                    }
                };
                if !font_data.is_empty() {
                    // If this is raw CFF data (FontFile3), wrap it in an OpenType
                    // container so ttf-parser can parse it.
                    let font_data = if *key == "FontFile3" && !font_data.is_empty()
                        && font_data[0] == 1 // CFF version 1
                        && font_data.len() > 4
                    {
                        log::info!(
                            "Font '{}': Wrapping raw CFF in OpenType container ({} bytes)",
                            base_font,
                            font_data.len()
                        );
                        wrap_cff_in_opentype(&font_data)
                    } else {
                        font_data
                    };
                    log::info!(
                        "Font '{}': Extracted embedded font from {} ({} bytes)",
                        base_font,
                        key,
                        font_data.len()
                    );
                    return (has_font_program, Some(Arc::new(font_data)));
                }
            }
        }
        (has_font_program, None)
    }
}

/// Wrap raw CFF font data in a minimal OpenType container so ttf-parser can parse it.
/// Creates an OpenType font with `head` and `CFF ` tables (both required by ttf-parser).
pub(super) fn wrap_cff_in_opentype(cff_data: &[u8]) -> Vec<u8> {
    let num_tables: u16 = 4; // CFF + head + hhea + maxp
    let search_range: u16 = 32; // largest power of 2 <= numTables*16 = 64 → 32 (2 tables)
    let entry_selector: u16 = 2;
    let range_shift: u16 = (num_tables * 16) - search_range;

    // Minimal head table (54 bytes) — OpenType spec required fields
    let head_table: [u8; 54] = [
        0x00, 0x01, 0x00, 0x00, // majorVersion=1, minorVersion=0
        0x00, 0x01, 0x00, 0x00, // fontRevision=1.0
        0x00, 0x00, 0x00, 0x00, // checksumAdjustment (0, will be ignored)
        0x5F, 0x0F, 0x3C, 0xF5, // magicNumber
        0x00, 0x0B, // flags (baseline at y=0, lsb at x=0, etc)
        0x03, 0xE8, // unitsPerEm = 1000
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // created (0)
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // modified (0)
        0xFF, 0x38, // xMin = -200
        0xFF, 0x38, // yMin = -200
        0x03, 0xE8, // xMax = 1000
        0x03, 0xE8, // yMax = 1000
        0x00, 0x00, // macStyle
        0x00, 0x08, // lowestRecPPEM = 8
        0x00, 0x02, // fontDirectionHint
        0x00, 0x01, // indexToLocFormat = 1 (long)
        0x00, 0x00, // glyphDataFormat
    ];

    // Minimal hhea table (36 bytes)
    let hhea_table: [u8; 36] = [
        0x00, 0x01, 0x00, 0x00, // majorVersion=1, minorVersion=0
        0x03, 0x20, // ascender = 800
        0xFF, 0x38, // descender = -200
        0x00, 0x00, // lineGap = 0
        0x04, 0x00, // advanceWidthMax = 1024
        0x00, 0x00, // minLeftSideBearing = 0
        0x00, 0x00, // minRightSideBearing = 0
        0x04, 0x00, // xMaxExtent = 1024
        0x00, 0x01, // caretSlopeRise = 1
        0x00, 0x00, // caretSlopeRun = 0
        0x00, 0x00, // caretOffset = 0
        0x00, 0x00, // reserved
        0x00, 0x00, // reserved
        0x00, 0x00, // reserved
        0x00, 0x00, // reserved
        0x00, 0x00, // metricDataFormat = 0
        0x01, 0x00, // numberOfHMetrics = 256
    ];

    // Minimal maxp table (6 bytes for CFF fonts — version 0.5)
    let maxp_table: [u8; 6] = [
        0x00, 0x00, 0x50, 0x00, // version = 0.5 (CFF)
        0x01, 0x00, // numGlyphs = 256
    ];

    // Layout: offset table (12) + 4 table records (64) = 76 bytes header
    let header_size: u32 = 12 + (num_tables as u32) * 16;
    // Place tables: head, hhea, maxp, CFF (alphabetical by tag within each group)
    let head_offset = (header_size + 3) & !3;
    let head_len = head_table.len() as u32;
    let hhea_offset = ((head_offset + head_len) + 3) & !3;
    let hhea_len = hhea_table.len() as u32;
    let maxp_offset = ((hhea_offset + hhea_len) + 3) & !3;
    let maxp_len = maxp_table.len() as u32;
    let cff_offset = ((maxp_offset + maxp_len) + 3) & !3;
    let cff_len = cff_data.len() as u32;

    fn table_checksum(data: &[u8]) -> u32 {
        let mut sum: u32 = 0;
        for chunk in data.chunks(4) {
            let mut bytes = [0u8; 4];
            bytes[..chunk.len()].copy_from_slice(chunk);
            sum = sum.wrapping_add(u32::from_be_bytes(bytes));
        }
        sum
    }

    let mut out = Vec::with_capacity((cff_offset + cff_len) as usize);

    // Offset table (12 bytes)
    out.extend_from_slice(b"OTTO");
    out.extend_from_slice(&num_tables.to_be_bytes());
    out.extend_from_slice(&search_range.to_be_bytes());
    out.extend_from_slice(&entry_selector.to_be_bytes());
    out.extend_from_slice(&range_shift.to_be_bytes());

    // Table record: CFF (alphabetical order: CFF before head)
    out.extend_from_slice(b"CFF ");
    out.extend_from_slice(&table_checksum(cff_data).to_be_bytes());
    out.extend_from_slice(&cff_offset.to_be_bytes());
    out.extend_from_slice(&cff_len.to_be_bytes());

    // Table record: head
    out.extend_from_slice(b"head");
    out.extend_from_slice(&table_checksum(&head_table).to_be_bytes());
    out.extend_from_slice(&head_offset.to_be_bytes());
    out.extend_from_slice(&head_len.to_be_bytes());

    // Table record: hhea
    out.extend_from_slice(b"hhea");
    out.extend_from_slice(&table_checksum(&hhea_table).to_be_bytes());
    out.extend_from_slice(&hhea_offset.to_be_bytes());
    out.extend_from_slice(&hhea_len.to_be_bytes());

    // Table record: maxp
    out.extend_from_slice(b"maxp");
    out.extend_from_slice(&table_checksum(&maxp_table).to_be_bytes());
    out.extend_from_slice(&maxp_offset.to_be_bytes());
    out.extend_from_slice(&maxp_len.to_be_bytes());

    // head table data
    while out.len() < head_offset as usize {
        out.push(0);
    }
    out.extend_from_slice(&head_table);

    // hhea table data
    while out.len() < hhea_offset as usize {
        out.push(0);
    }
    out.extend_from_slice(&hhea_table);

    // maxp table data
    while out.len() < maxp_offset as usize {
        out.push(0);
    }
    out.extend_from_slice(&maxp_table);

    // Pad to CFF offset
    while out.len() < cff_offset as usize {
        out.push(0);
    }

    // CFF data
    out.extend_from_slice(cff_data);

    out
}
