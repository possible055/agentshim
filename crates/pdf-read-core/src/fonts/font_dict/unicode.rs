use super::*;

impl FontInfo {
    /// Convert a character code to Unicode string.
    ///
    /// Returns the faithful Unicode mapping per PDF Spec §9.10.2. Ligature
    /// characters (U+FB00–FB06) are preserved here; expansion into component
    /// letters is done by the text pipeline via `LigatureDecisionMaker`, which
    /// inspects surrounding context (neighboring chars, word boundaries) to
    /// decide whether to split — keeping font_dict a pure encoding layer.
    pub fn char_to_unicode(&self, char_code: u32) -> Option<String> {
        // Serve from the per-font memo. Read and write are separate lock scopes
        // so the decode in between never holds the lock.
        if let Ok(memo) = self.type0_unicode_memo.lock() {
            if let Some(cached) = memo.get(&char_code) {
                return cached.clone();
            }
        }
        let result = self
            .char_to_unicode_uncached(char_code)
            .map(|s| normalize_cjk_radical_forms(&s));
        if let Ok(mut memo) = self.type0_unicode_memo.lock() {
            memo.insert(char_code, result.clone());
        }
        result
    }

    /// Uncached decode cascade behind [`Self::char_to_unicode`].
    pub(super) fn char_to_unicode_uncached(&self, char_code: u32) -> Option<String> {
        // char_code is now u32 to support 4-byte character codes (0x00000000-0xFFFFFFFF)
        // This is backward compatible - u16 values are automatically promoted to u32

        // ==================================================================================
        // PRIORITY 1: ToUnicode CMap (PDF Spec Section 9.10.2, Method 1)
        // ==================================================================================
        //
        // Per §9.10.2: if a ToUnicode CMap is PRESENT it is the authoritative source.
        // For composite (Type0) fonts a present-but-incomplete ToUnicode means the
        // unmapped codes genuinely have no Unicode equivalent. Falling through to the
        // predefined-CMap path (Priority 3 §9.10.2) for Type0 would guess wrong CJK
        // characters and score near zero versus ground truth. Therefore:
        //
        //   • ToUnicode hit → return the mapped string (or U+FFFD if it maps to FFFD
        //     or a bare C0 control character).
        //   • ToUnicode miss AND font is Type0 → return U+FFFD, do NOT fall through.
        //   • ToUnicode miss AND font is NOT Type0 → fall through to lower priorities
        //     (simple fonts with standard encoding still benefit from further lookup).
        //
        // Fix A (§9.10.2 Priority-3 guard): implemented in the CMap-miss branch below.
        // Fix B (control-character filter): applied on CMap hits.
        if let Some(lazy_cmap) = &self.to_unicode {
            if let Some(cmap) = lazy_cmap.get() {
                let raw_unicode = cmap.get(&char_code);
                let had_hit = raw_unicode.is_some();

                // For Identity-encoded fonts, a U+FFFD (notdefrange) or a BMP
                // noncharacter (U+FFFE / U+FFFF) result is NOT a definitive
                // mapping — some producers stuff these into ToUnicode as a
                // "no glyph" placeholder (arial_unicode_ab_cidfont maps every
                // CID to U+FFFF). The CID→GID→embedded-cmap / CID-as-Unicode
                // fallback below recovers the real character, so treat them as a
                // CMap miss. Noncharacters are permanently reserved and never
                // valid text, so this can only ever improve Identity-font output.
                let effective_hit = raw_unicode.filter(|u| {
                    let is_placeholder = !u.is_empty()
                        && u.chars()
                            .all(|c| matches!(c, '\u{FFFD}' | '\u{FFFE}' | '\u{FFFF}'));
                    !(is_placeholder && matches!(self.encoding, Encoding::Identity))
                });

                if let Some(unicode) = effective_hit {
                    // Fix B: filter bare C0 control characters (U+0000–U+001F except
                    // tab/LF/CR which are legitimate whitespace in extracted text).
                    // These are never valid visible text and typically indicate a
                    // broken ToUnicode entry. Return U+FFFD and do NOT fall through
                    // even for simple fonts — the CMap explicitly mapped this code.
                    let is_c0_control = unicode
                        .chars()
                        .all(|c| matches!(c as u32, 0x00..=0x08 | 0x0B..=0x0C | 0x0E..=0x1F))
                        && !unicode.is_empty();

                    if unicode.as_ref() == "\u{FFFD}" {
                        log::debug!(
                            "ToUnicode CMap has U+FFFD for code 0x{:02X} in font '{}' - returning U+FFFD",
                            char_code, self.base_font
                        );
                        return Some("\u{FFFD}".to_string());
                    } else if is_c0_control {
                        log::debug!(
                            "ToUnicode CMap maps code 0x{:04X} to C0 control char(s) in font '{}' - returning U+FFFD",
                            char_code, self.base_font
                        );
                        return Some("\u{FFFD}".to_string());
                    } else {
                        // Interception A (Item 1): glyph-name-gated punctuation
                        // recovery. When a present ToUnicode CMap resolves a code to
                        // a non-sensible symbol (e.g. U+00AC `¬`) but the font's
                        // authoritative glyph name for that code is punctuation
                        // (`period`/`comma`/`hyphen`/`minus` via /Differences or the
                        // embedded post/charset table), prefer the §9.10.2(a)+(b) AGL
                        // result. Gated so a correctly-mapped period (whose hit is
                        // already `.`) never enters here.
                        if is_non_sensible_symbol(&unicode) {
                            if let Some(glyph_name) = self.glyph_name_for_code(char_code) {
                                if let Some(punct) = punctuation_unicode_for_glyph_name(glyph_name)
                                {
                                    log::debug!(
                                        "Interception A: code 0x{:04X} ToUnicode '{}' is a non-sensible symbol; glyph name '{}' → '{}' (font '{}')",
                                        char_code, unicode, glyph_name, punct, self.base_font
                                    );
                                    return Some(punct.to_string());
                                }
                            }
                        }
                        return Some(unicode.into_owned());
                    }
                } else {
                    if had_hit {
                        log::debug!(
                            "Identity font '{}': notdefrange U+FFFD treated as miss for code 0x{:04X} — falling through to CID-as-Unicode",
                            self.base_font, char_code
                        );
                    } else {
                        log::debug!(
                            "ToUnicode CMap MISS: font='{}' subtype='{}' code=0x{:04X} (cmap has {} entries)",
                            self.base_font, self.subtype, char_code, cmap.len()
                        );
                    }

                    // Fix A (§9.10.2): for composite (Type0) fonts a present ToUnicode
                    // CMap is the authoritative mapping. A miss means the glyph has no
                    // Unicode equivalent — do NOT fall through to the predefined-CMap
                    // path which would produce plausible-looking but wrong CJK chars.
                    // Exception: Identity-encoded fonts map CID directly to Unicode, so
                    // a CMap miss still has a valid fallback (CID == Unicode codepoint).
                    // Blocking them here would suppress spaces and Latin characters.
                    if self.subtype == "Type0" && !matches!(self.encoding, Encoding::Identity) {
                        log::debug!(
                            "Type0 font '{}': ToUnicode present but code 0x{:04X} not covered → U+FFFD (no Priority-3 fallback per §9.10.2)",
                            self.base_font, char_code
                        );
                        return Some("\u{FFFD}".to_string());
                    }
                }
            } else {
                log::warn!(
                    "Failed to parse lazy CMap for font '{}' - will fall back to Priority 2",
                    self.base_font
                );
            }
        } else if self.subtype == "Type0" {
            log::debug!(
                "Type0 font '{}' missing ToUnicode CMap - will fall back to Priority 2",
                self.base_font
            );
        }

        // ==================================================================================
        // PRIORITY 2: Predefined CMaps (PDF Spec Section 9.7.5.2)
        // ==================================================================================
        // Phase 3.1: Identity-H/Identity-V Predefined CMap Support
        //
        // For CID-keyed fonts (Type0 subtype), predefined CMaps provide character mapping
        // when no ToUnicode CMap is present. This is critical for CJK PDFs using standard
        // Adobe CID collections (Adobe-Identity, Adobe-GB1, Adobe-Japan1, etc.)
        //
        // Identity-H/Identity-V: The simplest predefined CMap
        // - Maps 2-byte CID directly to 2-byte Unicode code point: CID == Unicode
        // - Used with ANY font when encoding is "Identity-H" or "Identity-V"
        // - Per PDF Spec ISO 32000-1:2008 Section 9.7.5.2
        //
        // Examples:
        // - CID 0x4E00 → U+4E00 (CJK UNIFIED IDEOGRAPH "一" in Chinese/Japanese)
        // - CID 0x0041 → U+0041 (Latin Capital Letter A)
        //
        // NOTE: Identity-H/V is actually handled by checking the encoding field.
        // It is checked here for Type0 fonts to ensure it happens before other fallbacks.
        if self.subtype == "Type0" {
            if let Encoding::Standard(ref encoding_name) = self.encoding {
                if encoding_name == "Identity-H"
                    || encoding_name == "Identity-V"
                    || encoding_name.contains("UCS2")
                    || encoding_name.contains("UTF16")
                {
                    // For Identity-H/V: CID value IS the Unicode code point (2-byte)
                    // Valid Unicode range for 2-byte CID: 0x0000 to 0xFFFF
                    // (Standard Unicode BMP - Basic Multilingual Plane)
                    // Since char_code is u16, it's always in range [0x0000, 0xFFFF]
                    //
                    // IMPORTANT: Per PDF Spec 9.10.2, Type0 fonts require either:
                    // 1. A ToUnicode CMap, OR
                    // 2. A predefined CMap (which requires CIDSystemInfo)
                    //
                    // If neither exists, we should NOT treat Identity-H/V as valid for Type0.
                    // This prevents "identity" treatment when there's no proper CIDSystemInfo.
                    if self.cid_system_info.is_some() {
                        // For Adobe-Identity ordering, CIDs are glyph indices (GIDs),
                        // NOT Unicode code points. Try the embedded TrueType cmap first.
                        let is_identity_ordering = self
                            .cid_system_info
                            .as_ref()
                            .map(|info| info.ordering == "Identity")
                            .unwrap_or(false);

                        if is_identity_ordering {
                            // Try TrueType cmap: CID → GID → Unicode
                            if let Some(tt_cmap) = self.truetype_cmap() {
                                let gid = if let Some(ref cid_to_gid) = self.cid_to_gid_map {
                                    cid_to_gid.get_gid(char_code as u16)
                                } else {
                                    char_code as u16
                                };
                                if let Some(unicode_char) = tt_cmap.get_unicode(gid) {
                                    return Some(unicode_char.to_string());
                                }
                            }
                        }

                        // For UCS2/UTF16 encodings, char codes ARE Unicode values directly.
                        // For Identity-H/V with non-Identity ordering (e.g., Adobe-GB1),
                        // char codes are CIDs that need CID-to-Unicode lookup.
                        let is_ucs2_or_utf16 =
                            encoding_name.contains("UCS2") || encoding_name.contains("UTF16");
                        let is_non_identity_ordering = self
                            .cid_system_info
                            .as_ref()
                            .map(|info| info.ordering != "Identity")
                            .unwrap_or(false);

                        if !is_ucs2_or_utf16 && is_non_identity_ordering {
                            // Identity-H/V with CJK collection: CIDs are NOT Unicode!
                            if let Some(unicode_codepoint) = lookup_predefined_cmap(
                                encoding_name,
                                &self.cid_system_info,
                                char_code as u16,
                            ) {
                                if let Some(unicode_char) = char::from_u32(unicode_codepoint) {
                                    return Some(unicode_char.to_string());
                                }
                            }
                            // CID lookup failed — fall through to Priority 2b and beyond
                        } else {
                            // UCS2/UTF16 or Adobe-Identity: char code == Unicode
                            if let Some(unicode_char) = char::from_u32(char_code) {
                                if !unicode_char.is_control() || unicode_char == ' ' {
                                    return Some(unicode_char.to_string());
                                }
                            }
                        }
                    } else {
                        // No CIDSystemInfo — use CID-as-Unicode as last resort.
                        // Many PDF generators assign CID values equal to Unicode code points
                        // even without proper CIDSystemInfo. MuPDF uses this fallback.
                        if let Some(unicode_char) = char::from_u32(char_code) {
                            if !unicode_char.is_control() || unicode_char == ' ' {
                                log::debug!(
                                    "Identity-H/V CID-as-Unicode fallback (no CIDSystemInfo): font='{}' CID=0x{:04X} → '{}' (U+{:04X})",
                                    self.base_font,
                                    char_code,
                                    unicode_char,
                                    unicode_char as u32
                                );
                                return Some(unicode_char.to_string());
                            }
                        }
                        log::debug!(
                            "Type0 font '{}' with {} encoding: CID 0x{:04X} is not a valid Unicode code point",
                            self.base_font,
                            encoding_name,
                            char_code
                        );
                    }
                }
            }
        }

        // ==================================================================================
        // PRIORITY 2a: Shift-JIS (RKSJ) direct decoding
        // ==================================================================================
        // For fonts using 90ms-RKSJ-H/V encoding, the char_code is a Shift-JIS value
        // (after byte grouping in decode_text_to_unicode). Convert directly to Unicode.
        if self.subtype == "Type0" {
            if let Encoding::Standard(ref enc) = self.encoding {
                if enc.contains("RKSJ") {
                    if let Some(unicode_char) = shift_jis_to_unicode(char_code as u16) {
                        return Some(unicode_char.to_string());
                    }
                }
            }
        }

        // ==================================================================================
        // PRIORITY 2b: Unicode-based Predefined CMaps (Phase 3.2)
        // ==================================================================================
        // For Type0 fonts without a ToUnicode CMap: follow PDF §9.10.2 priority order.
        //
        // The spec defines two distinct encoding CMap kinds:
        //
        //   (a) Byte-encoding CMaps (GBpc-EUC-H, GB-EUC-H, B5pc-H, EUC-H, KSC-EUC-H,
        //       etc.): the value in the content stream is a raw multi-byte code in a
        //       legacy encoding (GBK, EUC-CN, Big5, EUC-JP, EUC-KR). §9.10.2 says to
        //       map char code → CID first, but those encoding CMap tables are not
        //       embedded here. Decoding the raw bytes directly with encoding_rs is
        //       equivalent (same Unicode output) and is permitted by the spec's fallback
        //       clause: "there is no way to determine … a conforming reader may choose a
        //       character code of their choosing."
        //
        //   (b) Identity / UCS2 CMaps (Identity-H, UniGB-UCS2-H, etc.): the value in
        //       the content stream IS (or approximates) a CID. Use the Adobe-XX CID →
        //       Unicode table directly (§9.10.2 step b).
        //
        // `decode_cjk_raw_charcode` returns None for non-byte-encoding CMaps, so
        // trying it first is safe: it is a no-op for Identity/UCS2 fonts.
        if self.subtype == "Type0" {
            let enc_name = match &self.encoding {
                Encoding::Standard(name) => name.clone(),
                Encoding::Identity => "Identity-H".to_string(),
                Encoding::Custom(_) => String::new(),
            };

            // Step (a): try direct byte decode for legacy CJK byte-encoding CMaps.
            // This is the correct primary path for GBpc-EUC-H, GB-EUC-H, B5pc-H,
            // EUC-H, KSC-EUC-H, etc. Returns None for Identity/UCS2 CMaps, in
            // which case we fall through to the CID lookup below.
            if let Some(result) =
                decode_cjk_raw_charcode(char_code, &enc_name, &self.cid_system_info)
            {
                return Some(result);
            }

            // Step (b): CID → Unicode lookup for identity / UCS2 CMaps where the
            // char code in the stream is already a CID (or very close to one).
            if let Some(unicode_codepoint) =
                lookup_predefined_cmap(&enc_name, &self.cid_system_info, char_code as u16)
            {
                if let Some(unicode_char) = char::from_u32(unicode_codepoint) {
                    return Some(unicode_char.to_string());
                }
            }
        }

        // ==================================================================================
        // PRIORITY 2: Predefined Encodings (PDF Spec Section 9.10.2, Method 2)
        // ==================================================================================
        // For symbolic fonts (Flags bit 3 set), the PDF spec requires us to IGNORE any
        // /Encoding entry and use the font's built-in encoding directly.
        //
        // PDF Spec ISO 32000-1:2008, Section 9.6.6.1:
        // "For symbolic fonts, the Encoding entry is ignored; characters are mapped directly
        // using their character codes to glyphs in the font."
        //
        // Common symbolic fonts: Symbol (Greek/math), ZapfDingbats (decorative)
        if self.is_symbolic() {
            let font_name_lower = self.base_font.to_lowercase();

            // Symbol font: Maps character codes to Greek letters and mathematical symbols
            // Standard encoding defined in PDF spec Annex D.4
            if font_name_lower.contains("symbol") {
                if let Some(unicode_char) = symbol_encoding_lookup(char_code as u8) {
                    log::debug!(
                        "Symbolic font '{}': code 0x{:02X} → '{}' (U+{:04X}) [using Symbol encoding]",
                        self.base_font,
                        char_code,
                        unicode_char,
                        unicode_char as u32
                    );
                    return Some(unicode_char.to_string());
                }
            }
            // ZapfDingbats font: Maps character codes to decorative symbols
            // Standard encoding defined in PDF spec Annex D.5
            else if font_name_lower.contains("zapf") || font_name_lower.contains("dingbat") {
                if let Some(unicode_char) = zapf_dingbats_encoding_lookup(char_code as u8) {
                    log::debug!(
                        "Symbolic font '{}': code 0x{:02X} → '{}' (U+{:04X}) [using ZapfDingbats encoding]",
                        self.base_font,
                        char_code,
                        unicode_char,
                        unicode_char as u32
                    );
                    return Some(unicode_char.to_string());
                }
            }

            // For other symbolic fonts without specific encoding, fall through to /Encoding
            // (though spec says to ignore /Encoding, some PDFs may still work with it)
        }

        // ==================================================================================
        // PRIORITY 3: Font's /Encoding Entry (PDF Spec Section 9.10.2, Method 3)
        // ==================================================================================
        // For non-symbolic fonts, use the /Encoding entry which can be:
        // - A predefined encoding name (e.g., WinAnsiEncoding, MacRomanEncoding)
        // - A custom encoding dictionary with /BaseEncoding and /Differences array
        //
        // The /Differences array allows overriding specific character codes with custom
        // glyph names, which are then mapped to Unicode via the Adobe Glyph List (AGL).
        match &self.encoding {
            Encoding::Standard(name) => {
                // Check for Identity-H and Identity-V encodings (common for Type0 fonts)
                if name == "Identity-H" || name == "Identity-V" {
                    // NOTE: Type0 fonts with Identity-H/V are handled at Priority 2 (predefined CMaps)
                    // above, so this code path is only reached for simple fonts (Type1, TrueType).
                    // Type0 fonts will have already returned at Priority 2 if the CID is valid Unicode.
                    if self.subtype == "Type0" {
                        // Priority 2 didn't map this CID. Use CID-as-Unicode fallback.
                        if let Some(unicode_char) = char::from_u32(char_code) {
                            if !unicode_char.is_control() || unicode_char == ' ' {
                                log::debug!(
                                    "Type0 font '{}' {} encoding Priority 3 CID-as-Unicode: CID 0x{:04X} → '{}' (U+{:04X})",
                                    self.base_font,
                                    name,
                                    char_code,
                                    unicode_char,
                                    unicode_char as u32
                                );
                                return Some(unicode_char.to_string());
                            }
                        }
                        return Some("\u{FFFD}".to_string());
                    }
                    // For simple fonts, Identity encoding is valid
                    if let Some(ch) = char::from_u32(char_code) {
                        return Some(ch.to_string());
                    }
                }

                // For TrueType subset fonts with no /Encoding, character codes are often
                // GIDs (glyph indices), not standard encoding values. Per PDF Spec 9.6.5.4,
                // when no /Encoding exists and the font has a (3,1) cmap, character codes
                // map through the cmap. Try TrueType cmap first for these fonts.
                if (self.subtype == "TrueType" || self.subtype == "Type1")
                    && name == "StandardEncoding"
                {
                    if let Some(tt_cmap) = self.truetype_cmap() {
                        let gid = tt_cmap
                            .code_to_gid(char_code as u16)
                            .unwrap_or(char_code as u16);
                        if let Some(unicode_char) = tt_cmap.get_unicode(gid) {
                            return Some(unicode_char.to_string());
                        }
                    }
                }

                // Predefined encodings: StandardEncoding, WinAnsiEncoding, MacRomanEncoding, etc.
                if let Some(unicode) = standard_encoding_lookup(name, char_code as u8) {
                    log::debug!(
                        "Standard encoding '{}': code 0x{:02X} → '{}'",
                        name,
                        char_code,
                        unicode
                    );
                    return Some(unicode);
                }
            }
            Encoding::Custom(map) => {
                // Custom encoding with /Differences array
                // Maps character code → glyph name → Unicode (via AGL)
                if let Some(&custom_char) = map.get(&(char_code as u8)) {
                    log::debug!(
                        "Custom encoding: code 0x{:02X} → '{}' (U+{:04X})",
                        char_code,
                        custom_char,
                        custom_char as u32
                    );

                    // Interception B (Item 1): glyph-name-gated punctuation
                    // override. If the base/program encoding resolved this code to a
                    // non-sensible symbol but the /Differences glyph name is
                    // punctuation, the name is authoritative (ISO 32000-1 §9.6.6.1) —
                    // return the AGL punctuation so a `/period`-named code always wins
                    // as `.` regardless of how the resolved char came out.
                    if is_non_sensible_symbol(&custom_char.to_string()) {
                        if let Some(glyph_name) = self.diff_glyph_names.get(&(char_code as u8)) {
                            if let Some(punct) = punctuation_unicode_for_glyph_name(glyph_name) {
                                log::debug!(
                                    "Interception B: code 0x{:02X} resolved to non-sensible symbol '{}'; /Differences name '{}' → '{}' (font '{}')",
                                    char_code, custom_char, glyph_name, punct, self.base_font
                                );
                                return Some(punct.to_string());
                            }
                        }
                    }

                    // Handle ligatures (ff, fi, fl, ffi, ffl) by expanding to component characters
                    // This is NOT in the PDF spec but improves text extraction usability
                    if is_ligature_char(custom_char) {
                        if let Some(expanded) = expand_ligature_char(custom_char) {
                            return Some(expanded.to_string());
                        }
                    }

                    return Some(custom_char.to_string());
                }
                // Check multi_char_map for compound glyph names (e.g., f_f → "ff")
                if let Some(multi_str) = self.multi_char_map.get(&(char_code as u8)) {
                    return Some(multi_str.clone());
                }
            }
            Encoding::Identity => {
                // CRITICAL: Identity encoding assumes char_code == Unicode.
                // This is ONLY valid for simple fonts, NOT Type0/CID fonts.
                // Per PDF Spec ISO 32000-1:2008 Section 9.7.6.3:
                // "Type0 fonts REQUIRE ToUnicode CMaps for proper character mapping"

                if self.subtype == "Type0" {
                    // Type0 fonts: character codes are CID (glyph indices), NOT Unicode
                    // Per PDF Spec ISO 32000-1:2008 Section 9.7.4.2, when no ToUnicode CMap exists,
                    // conforming readers SHALL use the TrueType font's internal "cmap" table as fallback.
                    // This requires translating CID → GID via the CIDToGIDMap, then looking up Unicode.

                    if let Some(tt_cmap) = self.truetype_cmap() {
                        // Translate CID → GID using the CIDToGIDMap
                        // Note: CIDToGIDMap only works with u16 CIDs (2-byte codes)
                        // For CIDs > 0xFFFF, we skip CIDToGIDMap and use char_code as GID if it fits in u16
                        let gid = if char_code <= 0xFFFF {
                            if let Some(ref cid_to_gid) = self.cid_to_gid_map {
                                cid_to_gid.get_gid(char_code as u16)
                            } else {
                                // No explicit mapping - assume Identity (CID == GID)
                                char_code as u16
                            }
                        } else {
                            // Large CID (> 0xFFFF) - cannot use CIDToGIDMap
                            // GIDs are typically u16, so large CIDs won't map correctly
                            log::debug!(
                                "CID 0x{:X} in font '{}' is too large (> 0xFFFF) for CIDToGIDMap - skipping TrueType cmap",
                                char_code,
                                self.base_font
                            );
                            // Return early to skip TrueType cmap lookup for large CIDs
                            return None;
                        };

                        if let Some(unicode_char) = tt_cmap.get_unicode(gid) {
                            log::debug!(
                                "TrueType cmap fallback SUCCESS: font='{}' CID=0x{:04X} (GID={}) → '{}' (U+{:04X})",
                                self.base_font,
                                char_code,
                                gid,
                                unicode_char,
                                unicode_char as u32
                            );
                            return Some(unicode_char.to_string());
                        } else {
                            log::debug!(
                                "TrueType cmap: GID {} not found in font '{}' (CID 0x{:04X} mapped via {})",
                                gid,
                                self.base_font,
                                char_code,
                                if self.cid_to_gid_map.is_some() {
                                    "explicit CIDToGIDMap"
                                } else {
                                    "Identity mapping"
                                }
                            );
                        }

                        // ==========================================================================
                        // PRIORITY 3c (#535,): embedded post/charset glyph name → AGL+synth
                        // ==========================================================================
                        // Per ISO 32000-1:2008 §9.10.2 fallback chain, consult the embedded font
                        // program's own glyph-name table when the TrueType `cmap` reverse lookup
                        // misses. Common on PowerPoint/Acrobat-exported Type0 Identity-H subset
                        // fonts that strip the Unicode `cmap` but keep `post` Format 2 names —
                        // bullets and `fi`/`fl` ligatures only recover via this path. Mirrors
                        // pdf.js / MuPDF / PDFBox 3.x behaviour. The earlier `gid_to_standard_
                        // glyph_name` (P5) only knows hardcoded ASCII-range GID → name; the post
                        // table is the font's own authoritative source.
                        if let Some(glyph_name) = self.embedded_glyph_name(gid) {
                            if let Some(unicode) =
                                crate::fonts::character_mapper::glyph_name_to_unicode(glyph_name)
                            {
                                log::debug!(
                                    "Priority 3c (embedded post glyph name): font='{}' CID=0x{:04X} (GID={}) → '{}' → '{}'",
                                    self.base_font,
                                    char_code,
                                    gid,
                                    glyph_name,
                                    unicode,
                                );
                                return Some(unicode);
                            } else {
                                log::debug!(
                                    "Priority 3c: font='{}' GID={} → name='{}' but AGL/synth lookup failed",
                                    self.base_font,
                                    gid,
                                    glyph_name,
                                );
                            }
                        }
                    }

                    // ==================================================================================
                    // PRIORITY 5: Adobe Glyph List Fallback (Phase 1.2)
                    // ==================================================================================
                    // When TrueType cmap fails (or is not available), try Adobe Glyph List fallback.
                    // This handles Type0 fonts with standard glyph names (e.g., Aptos, LMRoman)
                    // that don't have ToUnicode CMaps or embedded TrueType fonts.
                    //
                    // Process: CID → GID (via CIDToGIDMap) → Glyph Name → Unicode (via AGL)
                    //
                    // IMPORTANT: Only apply AGL fallback if a CIDToGIDMap is explicitly defined
                    // (even if it's Identity). This distinguishes between:
                    // - Type0 fonts with proper CIDToGIDMap (may have standard glyphs)
                    // - Malformed Type0 fonts without CIDToGIDMap (unlikely to work)
                    //
                    // Per PDF Spec ISO 32000-1:2008 Section 9.10.2:
                    // "If a ToUnicode CMap is not available, conforming readers may fall back
                    // to predefined encodings and glyph name lookup."

                    // A present-but-empty /ToUnicode (0 bfchar/bfrange) maps nothing, so it
                    // counts as absent — otherwise an Identity-ordered font with an empty CMap
                    // would suppress the fallbacks below and drop all its text.
                    let has_usable_tounicode = self
                        .to_unicode
                        .as_ref()
                        .and_then(|c| c.get())
                        .is_some_and(|cmap| !cmap.is_empty());
                    let is_identity_ordered = self
                        .cid_system_info
                        .as_ref()
                        .map(|info| info.ordering == "Identity")
                        .unwrap_or(false);

                    // The GID→AGL fallback below is a numeric *guess*: it reads the GID as a
                    // codepoint via the standard glyph-name table → AGL. It is meaningless for
                    // Identity-ordered subset fonts, whose GIDs are arbitrary — a remapped GID
                    // lands on an unrelated punctuation name (e.g. "Justin" → "J)'(i#") and would
                    // shadow the CID-as-Unicode mapping below — so it is skipped there. With a
                    // usable /ToUnicode present a code reaching here is genuinely unmapped, so the
                    // guess is suppressed entirely — prefer U+FFFD so the gap is detectable.
                    if !has_usable_tounicode && !is_identity_ordered {
                        if let Some(ref cid_to_gid) = self.cid_to_gid_map {
                            // CIDToGIDMap only works with u16 CIDs (2-byte codes)
                            if char_code > 0xFFFF {
                                log::debug!(
                                    "CID 0x{:X} in font '{}' is too large (> 0xFFFF) for CIDToGIDMap AGL fallback - skipping",
                                    char_code,
                                    self.base_font
                                );
                                // Fall through to continue fallback attempts
                            } else {
                                let gid = cid_to_gid.get_gid(char_code as u16);

                                if let Some(glyph_name) = Self::gid_to_standard_glyph_name(gid) {
                                    if let Some(&unicode_char) = ADOBE_GLYPH_LIST.get(glyph_name) {
                                        log::debug!(
                                            "Adobe Glyph List fallback SUCCESS: font='{}' CID=0x{:04X} (GID={}) → glyph '{}' → '{}' (U+{:04X})",
                                            self.base_font,
                                            char_code,
                                            gid,
                                            glyph_name,
                                            unicode_char,
                                            unicode_char as u32
                                        );
                                        return Some(unicode_char.to_string());
                                    }
                                }
                            }
                        }
                    }

                    // CID-as-Unicode fallback: many producers assign CID == Unicode codepoint.
                    // Used when there is no usable /ToUnicode, and — for Identity-ordered fonts —
                    // also for uncovered whitespace (CID 0x20 → space, which producers routinely
                    // omit and is reliably U+0020; dropping it would wreck word boundaries). Any
                    // other uncovered CID in a font that *has* a /ToUnicode has no codepoint we can
                    // trust (e.g. a ligature subset slot), so it decodes to U+FFFD instead of a
                    // plausible-but-wrong, per-file-varying guess.
                    let identity_whitespace = is_identity_ordered && char_code == 0x20;
                    if !has_usable_tounicode || identity_whitespace {
                        if let Some(unicode_char) = char::from_u32(char_code) {
                            if !unicode_char.is_control() || unicode_char == ' ' {
                                log::debug!(
                                    "Type0 font '{}' Identity encoding CID-as-Unicode fallback: CID 0x{:04X} → '{}' (U+{:04X})",
                                    self.base_font,
                                    char_code,
                                    unicode_char,
                                    unicode_char as u32
                                );
                                return Some(unicode_char.to_string());
                            }
                        }
                    }
                    log::warn!(
                        "Type0 font '{}' using Identity encoding: CID 0x{:04X} could not be mapped to Unicode. \
                         Embedded font: {} bytes.",
                        self.base_font,
                        char_code,
                        self.embedded_font_data
                            .as_ref()
                            .map(|d| d.len())
                            .unwrap_or(0)
                    );
                    return Some("\u{FFFD}".to_string());
                }

                // For simple fonts (Type1, TrueType), Identity encoding MAY be valid
                if let Some(ch) = char::from_u32(char_code) {
                    log::debug!(
                        "Identity encoding (simple font '{}'): code 0x{:02X} → '{}' (U+{:04X})",
                        self.base_font,
                        char_code,
                        ch,
                        ch as u32
                    );
                    return Some(ch.to_string());
                }
            }
        }

        // ==================================================================================
        // PRIORITY 4: TrueType cmap fallback for simple fonts
        // ==================================================================================
        // When all encoding-based lookups fail, try the embedded TrueType cmap as a last
        // resort. For subset fonts, character codes may be GIDs that the encoding table
        // doesn't cover. The cmap provides GID → Unicode mapping.
        if self.subtype != "Type0" {
            if let Some(tt_cmap) = self.truetype_cmap() {
                // Symbolic TrueType fonts index glyphs by content byte through a
                // (3,0)/(1,0) symbol cmap, so the byte is not the GID. Resolve
                // byte→GID first; fall back to byte-as-GID when no symbol cmap.
                let gid = tt_cmap
                    .code_to_gid(char_code as u16)
                    .unwrap_or(char_code as u16);
                if let Some(unicode_char) = tt_cmap.get_unicode(gid) {
                    return Some(unicode_char.to_string());
                }
            }
        }

        // ==================================================================================
        // PRIORITY 5: Fallback - No Mapping Found
        // ==================================================================================
        // If we reach here, the character is either:
        // - A control character (0x00-0x1F, 0x7F-0x9F) - intentionally omitted
        // - A character code outside all known encodings
        // - From a malformed PDF missing encoding information
        //
        // Control characters don't have visible representations, so returning None
        // (which becomes empty string) is more appropriate than returning � (U+FFFD).
        log::debug!(
            "No Unicode mapping for font '{}' code=0x{:02X} (symbolic={}, encoding={:?}) - likely control char",
            self.base_font,
            char_code,
            self.is_symbolic(),
            self.encoding
        );

        // ==================================================================================
        // PRIORITY 6: Unicode Ligature Fallback
        // ==================================================================================
        // If no encoding mapping was found and the raw character code falls
        // in the Unicode ligature block (U+FB00-U+FB06), decompose into the
        // component letters. This is a pure-fallback codepath — when no
        // font data identifies the glyph, standard ligature decomposition
        // is the safest recovery. LaTeX and scientific PDF producers emit
        // these codes directly.
        let ligature_components = match char_code {
            0xFB00 => Some("ff"),
            0xFB01 => Some("fi"),
            0xFB02 => Some("fl"),
            0xFB03 => Some("ffi"),
            0xFB04 => Some("ffl"),
            0xFB05 | 0xFB06 => Some("st"),
            _ => None,
        };
        if let Some(s) = ligature_components {
            return Some(s.to_string());
        }

        None
    }
}
