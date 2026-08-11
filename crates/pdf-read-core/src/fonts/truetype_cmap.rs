use byteorder::{BigEndian, ReadBytesExt};
/// TrueType cmap table extraction for font character mapping
///
/// This module extracts Unicode mappings from TrueType font cmap tables,
/// providing a fallback for Type0 fonts missing ToUnicode CMaps.
///
/// The cmap table maps glyph IDs (GIDs) to Unicode code points.
/// We support formats 4 (BMP), 6 (trimmed), and 12 (Unicode full).
use std::collections::HashMap;
use std::io::Cursor;

/// Mac Roman (platform 1, encoding 0) high-half → Unicode mapping.
///
/// Bytes 0x00..0x7F are identical to ASCII and are handled by the caller.
/// This table covers 0x80..0xFF per Apple's Mac Roman → Unicode reference.
#[rustfmt::skip]
const MAC_ROMAN_HIGH: [char; 128] = [
    'Ä', 'Å', 'Ç', 'É', 'Ñ', 'Ö', 'Ü', 'á',
    'à', 'â', 'ä', 'ã', 'å', 'ç', 'é', 'è',
    'ê', 'ë', 'í', 'ì', 'î', 'ï', 'ñ', 'ó',
    'ò', 'ô', 'ö', 'õ', 'ú', 'ù', 'û', 'ü',
    '†', '°', '¢', '£', '§', '•', '¶', 'ß',
    '®', '©', '™', '´', '¨', '≠', 'Æ', 'Ø',
    '∞', '±', '≤', '≥', '¥', 'µ', '∂', '∑',
    '∏', 'π', '∫', 'ª', 'º', 'Ω', 'æ', 'ø',
    '¿', '¡', '¬', '√', 'ƒ', '≈', '∆', '«',
    '»', '…', '\u{00A0}', 'À', 'Ã', 'Õ', 'Œ', 'œ',
    '–', '—', '"', '"', '\'', '\'', '÷', '◊',
    'ÿ', 'Ÿ', '⁄', '€', '‹', '›', 'ﬁ', 'ﬂ',
    '‡', '·', '‚', '„', '‰', 'Â', 'Ê', 'Á',
    'Ë', 'È', 'Í', 'Î', 'Ï', 'Ì', 'Ó', 'Ô',
    '\u{F8FF}', 'Ò', 'Ú', 'Û', 'Ù', 'ı', 'ˆ', '˜',
    '¯', '˘', '˙', '˚', '¸', '˝', '˛', 'ˇ',
];

#[inline]
fn mac_roman_to_unicode(byte: u8) -> char {
    debug_assert!(byte >= 0x80);
    MAC_ROMAN_HIGH[(byte - 0x80) as usize]
}

/// Represents a TrueType cmap table extracted from an embedded font
#[derive(Debug, Clone)]
pub struct TrueTypeCMap {
    /// Mapping from Glyph ID to Unicode character
    gid_to_unicode: HashMap<u16, char>,
    /// Content-byte → GID, from a `(3,0)`/`(1,0)` subtable. Empty unless the
    /// font is symbolic-style; lets decode do byte→GID before GID→Unicode.
    symbol_code_to_gid: HashMap<u32, u16>,
}

impl TrueTypeCMap {
    /// Parse TrueType cmap table from font data
    ///
    /// The TrueType sfnt structure contains a directory of tables.
    /// We locate the 'cmap' table and parse the best available subtable.
    ///
    /// Priority for cmap subtables:
    /// 1. Platform 3 (Windows), Encoding 10 (Unicode full repertoire) - supports all Unicode
    /// 2. Platform 3 (Windows), Encoding 1 (Unicode BMP) - supports basic multilingual plane
    /// 3. Platform 0 (Unicode), Encoding 3 - fallback to old Unicode platform
    pub fn from_font_data(data: &[u8]) -> Result<Self, String> {
        let mut cursor = Cursor::new(data);

        // Parse sfnt header to locate table directory
        let (num_tables, search_range, entry_selector, range_shift) =
            Self::parse_sfnt_header(&mut cursor)?;

        // Find cmap table entry in the directory
        let cmap_offset = Self::find_cmap_table(
            &mut cursor,
            num_tables,
            search_range,
            entry_selector,
            range_shift,
        )?;

        // Parse cmap table and find the best subtable
        cursor.set_position(cmap_offset as u64);
        let cmap_version = cursor
            .read_u16::<BigEndian>()
            .map_err(|e| format!("Failed to read cmap version: {}", e))?;

        if cmap_version != 0 {
            return Err(format!("Unsupported cmap table version: {}", cmap_version));
        }

        let num_subtables = cursor
            .read_u16::<BigEndian>()
            .map_err(|e| format!("Failed to read cmap subtable count: {}", e))?;

        // Read all subtable records and pick the best Unicode subtable for the
        // GID→Unicode map.
        let mut best_subtable: Option<(u32, u32, u32)> = None; // (platform_id, encoding_id, offset)
        let mut best_priority = -1i32;

        for _ in 0..num_subtables {
            let platform_id = cursor
                .read_u16::<BigEndian>()
                .map_err(|e| format!("Failed to read platform ID: {}", e))?;
            let encoding_id = cursor
                .read_u16::<BigEndian>()
                .map_err(|e| format!("Failed to read encoding ID: {}", e))?;
            let offset = cursor
                .read_u32::<BigEndian>()
                .map_err(|e| format!("Failed to read subtable offset: {}", e))?;

            let priority = match (platform_id, encoding_id) {
                (3, 10) => 30, // Windows, Unicode full repertoire
                (3, 1) => 20,  // Windows, Unicode BMP
                (0, 3) => 10,  // Unicode platform, Unicode 2.0
                _ => 0,
            };

            if priority > best_priority {
                best_priority = priority;
                best_subtable = Some((platform_id as u32, encoding_id as u32, offset));
            }
        }

        let (platform_id, encoding_id, subtable_offset) =
            best_subtable.ok_or_else(|| "No suitable cmap subtable found".to_string())?;

        log::debug!(
            "TrueType cmap: selected platform={} encoding={} offset={}",
            platform_id,
            encoding_id,
            subtable_offset
        );

        cursor.set_position((cmap_offset + subtable_offset) as u64);
        let gid_to_unicode = Self::parse_cmap_subtable(&mut cursor)?;

        Ok(TrueTypeCMap {
            gid_to_unicode,
            // Symbolic fonts index glyphs by content byte, not GID — build
            // byte→GID from the (3,0)/(1,0) subtable so decode can do that hop.
            symbol_code_to_gid: Self::build_symbol_code_to_gid(data),
        })
    }

    /// Build a content-byte→GID map from the font's `(3,0)` symbol or `(1,0)`
    /// Macintosh cmap subtable, resolving the `0xF000` symbol-PUA offset at build
    /// time. Reuses `ttf-parser` (already a dependency). Empty when the font has
    /// no such subtable or fails to parse — decode then treats the byte as a GID.
    fn build_symbol_code_to_gid(data: &[u8]) -> HashMap<u32, u16> {
        use ttf_parser::PlatformId;
        let mut map = HashMap::new();
        let Ok(face) = ttf_parser::Face::parse(data, 0) else {
            return map;
        };
        let Some(cmap) = face.tables().cmap else {
            return map;
        };
        let mut chosen = None;
        let mut best = 0;
        for sub in cmap.subtables {
            let pri = match (sub.platform_id, sub.encoding_id) {
                (PlatformId::Windows, 0) => 2,   // symbol
                (PlatformId::Macintosh, 0) => 1, // Roman
                _ => 0,
            };
            if pri > best {
                best = pri;
                chosen = Some(sub);
            }
        }
        if let Some(sub) = chosen {
            for code in 0u32..256 {
                if let Some(gid) = sub
                    .glyph_index(code)
                    .or_else(|| sub.glyph_index(0xF000 | code))
                {
                    map.insert(code, gid.0);
                }
            }
        }
        map
    }

    /// Content-byte → GID via the font's symbol/Mac cmap. `None` when the font
    /// has no such subtable (decode then falls back to treating the byte as a GID).
    pub fn code_to_gid(&self, code: u16) -> Option<u16> {
        self.symbol_code_to_gid.get(&(code as u32)).copied()
    }

    /// Get Unicode character for a glyph ID
    pub fn get_unicode(&self, gid: u16) -> Option<char> {
        self.gid_to_unicode.get(&gid).copied()
    }

    /// Get the number of glyph mappings
    pub fn len(&self) -> usize {
        self.gid_to_unicode.len()
    }

    /// Check if cmap is empty
    pub fn is_empty(&self) -> bool {
        self.gid_to_unicode.is_empty()
    }

    // ==================================================================================
    // Private Helper Methods
    // ==================================================================================

    fn parse_sfnt_header(cursor: &mut Cursor<&[u8]>) -> Result<(u16, u16, u16, u16), String> {
        // Read sfnt version (4 bytes - can be 0x00010000 for TrueType or "OTTO" for OpenType)
        let version = cursor
            .read_u32::<BigEndian>()
            .map_err(|e| format!("Failed to read sfnt version: {}", e))?;

        // 0x00010000 = TrueType, 0x4F54544F = OpenType (OTTO), 0x74727565 = Apple TrueType ("true")
        if version != 0x00010000 && version != 0x4F54544F && version != 0x74727565 {
            // 0x4F54544F = "OTTO" (OpenType)
            return Err(format!("Invalid sfnt version: 0x{:08X}", version));
        }

        let num_tables = cursor
            .read_u16::<BigEndian>()
            .map_err(|e| format!("Failed to read table count: {}", e))?;
        let search_range = cursor
            .read_u16::<BigEndian>()
            .map_err(|e| format!("Failed to read search range: {}", e))?;
        let entry_selector = cursor
            .read_u16::<BigEndian>()
            .map_err(|e| format!("Failed to read entry selector: {}", e))?;
        let range_shift = cursor
            .read_u16::<BigEndian>()
            .map_err(|e| format!("Failed to read range shift: {}", e))?;

        Ok((num_tables, search_range, entry_selector, range_shift))
    }

    fn find_cmap_table(
        cursor: &mut Cursor<&[u8]>,
        num_tables: u16,
        _search_range: u16,
        _entry_selector: u16,
        _range_shift: u16,
    ) -> Result<u32, String> {
        // Linear search through table directory for 'cmap' tag (0x636D6170)
        const CMAP_TAG: u32 = 0x636D6170;

        for _ in 0..num_tables {
            let tag = cursor
                .read_u32::<BigEndian>()
                .map_err(|e| format!("Failed to read table tag: {}", e))?;
            let _checksum = cursor
                .read_u32::<BigEndian>()
                .map_err(|e| format!("Failed to read table checksum: {}", e))?;
            let offset = cursor
                .read_u32::<BigEndian>()
                .map_err(|e| format!("Failed to read table offset: {}", e))?;
            let _length = cursor
                .read_u32::<BigEndian>()
                .map_err(|e| format!("Failed to read table length: {}", e))?;

            if tag == CMAP_TAG {
                return Ok(offset);
            }
        }

        Err("cmap table not found in font".to_string())
    }

    fn parse_cmap_subtable(cursor: &mut Cursor<&[u8]>) -> Result<HashMap<u16, char>, String> {
        let format = cursor
            .read_u16::<BigEndian>()
            .map_err(|e| format!("Failed to read cmap format: {}", e))?;

        match format {
            0 => Self::parse_cmap_format0(cursor),
            4 => Self::parse_cmap_format4(cursor),
            6 => Self::parse_cmap_format6(cursor),
            12 => Self::parse_cmap_format12(cursor),
            _ => Err(format!("Unsupported cmap format: {}", format)),
        }
    }

    /// Parse cmap format 0 (legacy 1-byte indexed, Mac Roman era).
    ///
    /// Structure per Apple TrueType reference (subtable length 262):
    ///   u16 format       (already read by caller — 2 bytes)
    ///   u16 length       (262: 2+2+2+256)
    ///   u16 language
    ///   u8  glyphIdArray[256]   — glyphId for each byte code 0..255
    ///
    /// Microsoft Office subset fonts (Calibri, Times New Roman subsets in
    /// Word/Excel exports) still ship with a format-0 cmap for the `(1,0)`
    /// Macintosh encoding alongside their `(3,1)` Unicode cmap. When the
    /// Unicode cmap is missing or malformed we fall back to this one; the
    /// byte code acts as the Mac Roman character code.
    ///
    /// Benchmark canary: ~8 MS Office corpus fixtures previously logged
    /// "Unsupported cmap format: 0" warnings and lost font glyph mapping
    /// as a consequence (B9).
    fn parse_cmap_format0(cursor: &mut Cursor<&[u8]>) -> Result<HashMap<u16, char>, String> {
        let _length = cursor
            .read_u16::<BigEndian>()
            .map_err(|e| format!("Failed to read format 0 length: {}", e))?;
        let _language = cursor
            .read_u16::<BigEndian>()
            .map_err(|e| format!("Failed to read format 0 language: {}", e))?;

        let mut glyph_ids = [0u8; 256];
        std::io::Read::read_exact(cursor, &mut glyph_ids)
            .map_err(|e| format!("Failed to read format 0 glyphIdArray: {}", e))?;

        let mut gid_to_unicode = HashMap::new();
        for (byte, &gid) in glyph_ids.iter().enumerate() {
            if gid == 0 {
                continue;
            }
            // ASCII pass-through for 0x00..0x7F (Mac Roman lower half is
            // identical to ASCII). Above 0x7F we route through the Mac
            // Roman → Unicode table so byte 0x8A (a-umlaut in Mac Roman)
            // maps to U+00E4 instead of U+008A.
            let ch = if byte < 0x80 {
                char::from_u32(byte as u32)
            } else {
                Some(mac_roman_to_unicode(byte as u8))
            };
            if let Some(ch) = ch {
                gid_to_unicode.insert(gid as u16, ch);
            }
        }
        Ok(gid_to_unicode)
    }

    /// Parse cmap format 4 (BMP - supports characters U+0000 to U+FFFF)
    fn parse_cmap_format4(cursor: &mut Cursor<&[u8]>) -> Result<HashMap<u16, char>, String> {
        let _length = cursor
            .read_u16::<BigEndian>()
            .map_err(|e| format!("Failed to read format 4 length: {}", e))?
            as u32;
        let _language = cursor
            .read_u16::<BigEndian>()
            .map_err(|e| format!("Failed to read format 4 language: {}", e))?;

        let seg_count_x2 = cursor
            .read_u16::<BigEndian>()
            .map_err(|e| format!("Failed to read segCountX2: {}", e))?
            as usize;
        let seg_count = seg_count_x2 / 2;

        // Skip binary search parameters
        let _search_range = cursor
            .read_u16::<BigEndian>()
            .map_err(|e| format!("Failed to read searchRange: {}", e))?;
        let _entry_selector = cursor
            .read_u16::<BigEndian>()
            .map_err(|e| format!("Failed to read entrySelector: {}", e))?;
        let _range_shift = cursor
            .read_u16::<BigEndian>()
            .map_err(|e| format!("Failed to read rangeShift: {}", e))?;

        // Read segment arrays
        let mut end_codes = vec![0u16; seg_count];
        for i in 0..seg_count {
            end_codes[i] = cursor
                .read_u16::<BigEndian>()
                .map_err(|e| format!("Failed to read endCode[{}]: {}", i, e))?;
        }

        // Reserved pad
        let _reserved = cursor
            .read_u16::<BigEndian>()
            .map_err(|e| format!("Failed to read reserved pad: {}", e))?;

        let mut start_codes = vec![0u16; seg_count];
        for i in 0..seg_count {
            start_codes[i] = cursor
                .read_u16::<BigEndian>()
                .map_err(|e| format!("Failed to read startCode[{}]: {}", i, e))?;
        }

        let mut id_deltas = vec![0i16; seg_count];
        for i in 0..seg_count {
            id_deltas[i] = cursor
                .read_i16::<BigEndian>()
                .map_err(|e| format!("Failed to read idDelta[{}]: {}", i, e))?;
        }

        // id_range_offsets require special parsing - just read as array
        let mut id_range_offsets = vec![0u16; seg_count];
        for i in 0..seg_count {
            id_range_offsets[i] = cursor
                .read_u16::<BigEndian>()
                .map_err(|e| format!("Failed to read idRangeOffset[{}]: {}", i, e))?;
        }

        // Read remaining bytes as glyphIdArray (used when idRangeOffset != 0)
        let mut glyph_id_array = Vec::new();
        while let Ok(val) = cursor.read_u16::<BigEndian>() {
            glyph_id_array.push(val);
        }

        // Build character to GID mappings
        let mut gid_to_unicode = HashMap::new();

        for seg in 0..seg_count {
            let start = start_codes[seg] as u32;
            let end = end_codes[seg] as u32;
            let id_delta = id_deltas[seg] as i32;

            for char_code in start..=end {
                if char_code == 0xFFFF {
                    break; // End segment marker
                }

                let gid = if id_range_offsets[seg] == 0 {
                    // Simple formula: GID = charCode + idDelta
                    (char_code as i32 + id_delta) as u16
                } else {
                    // Per TrueType spec: index into glyphIdArray
                    // offset = idRangeOffset[i]/2 + (charCode - startCode[i]) + i - segCount
                    let offset = (id_range_offsets[seg] as usize) / 2
                        + (char_code as usize - start as usize)
                        + seg
                        - seg_count;
                    if offset < glyph_id_array.len() {
                        let raw = glyph_id_array[offset];
                        if raw != 0 {
                            (raw as i32 + id_delta) as u16
                        } else {
                            0
                        }
                    } else {
                        0
                    }
                };

                if gid != 0 {
                    if let Some(ch) = char::from_u32(char_code) {
                        gid_to_unicode.insert(gid, ch);
                    }
                }
            }
        }

        Ok(gid_to_unicode)
    }

    /// Parse cmap format 6 (trimmed table)
    fn parse_cmap_format6(cursor: &mut Cursor<&[u8]>) -> Result<HashMap<u16, char>, String> {
        let _length = cursor
            .read_u16::<BigEndian>()
            .map_err(|e| format!("Failed to read format 6 length: {}", e))?;
        let _language = cursor
            .read_u16::<BigEndian>()
            .map_err(|e| format!("Failed to read format 6 language: {}", e))?;

        let first_code = cursor
            .read_u16::<BigEndian>()
            .map_err(|e| format!("Failed to read firstCode: {}", e))?;
        let count = cursor
            .read_u16::<BigEndian>()
            .map_err(|e| format!("Failed to read entryCount: {}", e))? as usize;

        let mut gid_to_unicode = HashMap::new();

        for i in 0..count {
            let gid = cursor
                .read_u16::<BigEndian>()
                .map_err(|e| format!("Failed to read glyphId[{}]: {}", i, e))?;

            let char_code = first_code as u32 + i as u32;
            if let Some(ch) = char::from_u32(char_code) {
                gid_to_unicode.insert(gid, ch);
            }
        }

        Ok(gid_to_unicode)
    }

    /// Parse cmap format 12 (segmented coverage - supports full Unicode)
    fn parse_cmap_format12(cursor: &mut Cursor<&[u8]>) -> Result<HashMap<u16, char>, String> {
        // Skip reserved bytes
        let _reserved = cursor
            .read_u16::<BigEndian>()
            .map_err(|e| format!("Failed to read reserved: {}", e))?;

        let _length = cursor
            .read_u32::<BigEndian>()
            .map_err(|e| format!("Failed to read format 12 length: {}", e))?;
        let _language = cursor
            .read_u32::<BigEndian>()
            .map_err(|e| format!("Failed to read format 12 language: {}", e))?;

        let num_groups = cursor
            .read_u32::<BigEndian>()
            .map_err(|e| format!("Failed to read numGroups: {}", e))?
            as usize;

        let mut gid_to_unicode = HashMap::new();

        for _ in 0..num_groups {
            let start_char_code = cursor
                .read_u32::<BigEndian>()
                .map_err(|e| format!("Failed to read startCharCode: {}", e))?;
            let end_char_code = cursor
                .read_u32::<BigEndian>()
                .map_err(|e| format!("Failed to read endCharCode: {}", e))?;
            let start_gid = cursor
                .read_u32::<BigEndian>()
                .map_err(|e| format!("Failed to read startGlyphId: {}", e))?;

            for (offset, char_code) in (start_char_code..=end_char_code).enumerate() {
                let gid = (start_gid + offset as u32) as u16;
                if let Some(ch) = char::from_u32(char_code) {
                    gid_to_unicode.insert(gid, ch);
                }
            }
        }

        Ok(gid_to_unicode)
    }
}

#[cfg(test)]
mod tests;
