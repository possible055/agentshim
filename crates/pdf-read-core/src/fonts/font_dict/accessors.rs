use super::*;

impl FontInfo {
    /// Get the TrueType cmap, lazily extracting it on first access.
    /// Returns `None` if the font is not TrueType or has no embedded data.
    pub fn truetype_cmap(&self) -> Option<&TrueTypeCMap> {
        self.truetype_cmap
            .get_or_init(|| {
                if !self.is_truetype_font {
                    return None;
                }
                let font_data = self.embedded_font_data.as_ref()?;
                if font_data.is_empty() {
                    return None;
                }
                match TrueTypeCMap::from_font_data(font_data) {
                    Ok(cmap) if !cmap.is_empty() => {
                        log::info!(
                            "Lazy-extracted TrueType cmap for font '{}': {} mappings",
                            self.base_font,
                            cmap.len()
                        );
                        Some(cmap)
                    }
                    Ok(_) => None,
                    Err(e) => {
                        log::warn!(
                            "Font '{}': TrueType cmap extraction failed: {}",
                            self.base_font,
                            e
                        );
                        None
                    }
                }
            })
            .as_ref()
    }

    /// Set the TrueType cmap directly (used by share_truetype_cmaps and tests).
    pub fn set_truetype_cmap(&mut self, cmap: Option<TrueTypeCMap>) {
        self.truetype_cmap = std::sync::OnceLock::new();
        if let Some(c) = cmap {
            let _ = self.truetype_cmap.set(Some(c));
        } else {
            let _ = self.truetype_cmap.set(None);
        }
    }

    /// Check if a TrueType cmap is available (either already extracted or extractable).
    pub fn has_truetype_cmap(&self) -> bool {
        self.truetype_cmap().is_some()
    }

    /// The most authoritative Unicode-mapping resource this font offers, as a
    /// [`MappingProvenance`](crate::fonts::MappingProvenance).
    ///
    /// This is a **fact** derived from the font's structure — which mapping
    /// resources exist — not a decode of any particular character code. It
    /// mirrors the ISO 32000-1 §9.10.2 priority order and covers every font
    /// type, so it is complete where a font-type-specific structural check is
    /// not.
    ///
    /// [`Fallback`](crate::fonts::MappingProvenance::Fallback) is the important
    /// value: it means the font carries **no** mapping resource — no usable
    /// `/ToUnicode`, no predefined CID→Unicode collection, no embedded `cmap`,
    /// and no simple-font encoding — so any Unicode extracted for its glyphs is
    /// a fabricated echo, not read from the file (§9.10.2: "there is no way to
    /// determine what the character code represents"). Callers compose their own
    /// policy from this (route to OCR, flag the page, keep the raw echo).
    pub fn best_mapping_provenance(&self) -> crate::fonts::MappingProvenance {
        use crate::fonts::MappingProvenance as P;
        // 1. A present, non-empty /ToUnicode CMap is authoritative (§9.10.2).
        if self
            .to_unicode
            .as_ref()
            .and_then(|c| c.get())
            .is_some_and(|m| !m.is_empty())
        {
            return P::ToUnicode;
        }
        // 2. A predefined CID→Unicode collection: a Type0 font whose descendant
        //    uses a known, non-Identity ordering (Adobe-GB1/CNS1/Japan1/Korea1).
        if self.subtype == "Type0" {
            if let Some(info) = &self.cid_system_info {
                if info.ordering != "Identity" && !info.ordering.is_empty() {
                    return P::PredefinedCMap;
                }
            }
        }
        // 3. The embedded program's own cmap (recoverable byte-as-GID / Identity
        //    subsets that kept a usable cmap).
        if self.has_truetype_cmap() {
            return P::EmbeddedCmap;
        }
        // 4. A simple font resolves through its /Encoding → glyph name → AGL, and
        //    symbolic Symbol/ZapfDingbats through their built-in encodings.
        if self.subtype != "Type0" {
            return P::EncodingName;
        }
        // 5. A Type0 font with none of the above severs every path to Unicode.
        P::Fallback
    }

    /// Look up the embedded font program's `post`-table glyph name for the
    /// given GID.
    ///
    /// Lazily parses the embedded TrueType/OpenType font (via `ttf-parser`)
    /// on first access, then caches a `Vec<Option<String>>` indexed by GID
    /// for O(1) subsequent lookups. The parsed font's `Face::glyph_name`
    /// abstracts over TrueType `post` Format 2 names and CFF `charset` SIDs,
    /// so this works for both TrueType (FontFile2) and CFF / Type1C
    /// (FontFile3) subset fonts.
    ///
    /// Returns `None` when:
    /// - the font has no embedded program (`embedded_font_data == None`),
    /// - the font program is empty or fails to parse,
    /// - the `post` table is Format 3 (no names) or the GID is out of range,
    /// - the parsed name is `.notdef` (which AGL doesn't map and isn't
    ///   useful as text anyway).
    ///
    /// Used by §9.10.2 Priority 3c in `decode_char_to_unicode`.
    pub(crate) fn embedded_glyph_name(&self, gid: u16) -> Option<&str> {
        let names = self
            .embedded_glyph_names
            .get_or_init(|| {
                let font_data = self.embedded_font_data.as_ref()?;
                if font_data.is_empty() {
                    return None;
                }
                let face = match ttf_parser::Face::parse(font_data, 0) {
                    Ok(f) => f,
                    Err(e) => {
                        log::debug!(
                            "Font '{}': ttf-parser Face::parse failed for glyph-name extraction: {:?}",
                            self.base_font,
                            e
                        );
                        return None;
                    },
                };
                let n = face.number_of_glyphs();
                // `number_of_glyphs` returns u16; cap the vec at that size.
                let mut out: Vec<Option<String>> = Vec::with_capacity(n as usize);
                let mut found_any = false;
                for g in 0..n {
                    let name = face
                        .glyph_name(ttf_parser::GlyphId(g))
                        .filter(|s| !s.is_empty() && *s != ".notdef")
                        .map(|s| s.to_string());
                    if name.is_some() {
                        found_any = true;
                    }
                    out.push(name);
                }
                if !found_any {
                    log::debug!(
                        "Font '{}': embedded program has no usable glyph names (post Format 3 or stripped)",
                        self.base_font
                    );
                    return None;
                }
                log::info!(
                    "Font '{}': cached {} embedded glyph names (post/charset) for §9.10.2 Priority 3c fallback",
                    self.base_font,
                    out.iter().filter(|n| n.is_some()).count(),
                );
                Some(out)
            })
            .as_ref()?;
        names.get(gid as usize).and_then(|n| n.as_deref())
    }

    /// Authoritative glyph name for a *simple* font character code, in priority
    /// order (ISO 32000-1 §9.6.6.1 / §9.10.2):
    /// (a) the `/Differences` glyph name retained in `diff_glyph_names`;
    /// (b) else the embedded post/charset glyph name for the code's GID
    ///     (`embedded_glyph_name`), when the embedded program carries names.
    ///
    /// Used by the Item 1 punctuation-recovery interceptions in `char_to_unicode`.
    pub(super) fn glyph_name_for_code(&self, char_code: u32) -> Option<&str> {
        if let Some(name) = self.diff_glyph_names.get(&(char_code as u8)) {
            return Some(name.as_str());
        }
        // Fall back to the embedded program's glyph name for this code's GID.
        // For embedded CFF subsets the byte_code → GID map is authoritative;
        // otherwise treat the code as the GID (TrueType simple-font convention).
        let gid = self
            .cff_gid_map
            .as_ref()
            .and_then(|m| m.get(&(char_code as u8)).copied())
            .unwrap_or(char_code as u16);
        self.embedded_glyph_name(gid)
    }
}
