use super::parsing::*;
use super::preflight::*;
use super::*;

impl PdfDocument {
    /// Like [`Self::extract_embedded_fonts_with_unicode_maps`] but also
    /// returns the per-glyph widths from the source PDF's `/W` array
    /// (in 1/1000 em units, keyed by GID). Required for re-embedding
    /// CFF font subsets whose synthetic OpenType wrapper carries no
    /// `hmtx` table — without this, ttf-parser returns 0 for every
    /// glyph advance and the round-trip writer emits a `/W` of zeros.
    pub fn extract_embedded_fonts_with_unicode_maps_and_widths(
        &self,
    ) -> Result<
        Vec<(
            String,
            Vec<u8>,
            std::collections::HashMap<u32, u16>,
            std::collections::HashMap<u16, u16>,
        )>,
    > {
        use std::collections::HashMap;
        let mut by_name: HashMap<String, (Vec<u8>, HashMap<u32, u16>, HashMap<u16, u16>)> =
            HashMap::new();

        let n = self.page_count()?;
        for page_idx in 0..n {
            let resources = match self.get_page(page_idx) {
                Ok(page) => match page.as_dict() {
                    Some(d) => {
                        let r = d
                            .get("Resources")
                            .cloned()
                            .unwrap_or(Object::Dictionary(std::collections::HashMap::new()));
                        if let Some(rref) = r.as_reference() {
                            self.load_object(rref).unwrap_or_else(|_| {
                                Object::Dictionary(std::collections::HashMap::new())
                            })
                        } else {
                            r
                        }
                    }
                    None => continue,
                },
                Err(_) => continue,
            };
            let mut extractor = crate::extractors::TextExtractor::new();
            if self.load_fonts_public(&resources, &mut extractor).is_err() {
                continue;
            }
            for (_resource_name, font_arc) in extractor.get_font_set() {
                let Some(data) = font_arc.embedded_font_data.as_ref() else {
                    continue;
                };
                if data.is_empty() {
                    continue;
                }
                let base = font_arc.base_font.as_str();
                let canonical = base.split_once('+').map(|(_, rest)| rest).unwrap_or(base);

                // Build Unicode → GID via ToUnicode CMap + GID resolver.
                //
                // We must consult the ToUnicode CMap *directly* rather than
                // going through `char_to_unicode`. `char_to_unicode` falls
                // through to a CID-as-Unicode fallback when the ToUnicode
                // CMap has no entry for a given code (Identity-H + Adobe-
                // Identity ordering, source font without a Unicode cmap).
                // That fallback returns spurious mappings like
                // U+0069 'i' → GID 105 (because CID 105 has no real
                // ToUnicode entry; the CID-as-Unicode path yields 'i'
                // for code=105 and the embedded TTF has no cmap to set us
                // straight). The spurious entries overwrite the real ones
                // we collected from CIDs that *do* have ToUnicode
                // entries (e.g. CID 0x4C → 'i', GID 76 for a
                // MicrosoftSansSerif subset) — which then makes the
                // injected cmap point Unicode codepoints at the wrong
                // glyph slots and the DOCX round-trip renders broken
                // lowercase letters.
                let mut uni_to_gid: HashMap<u32, u16> = HashMap::new();
                let to_unicode_cmap = font_arc.to_unicode.as_ref().and_then(|lazy| lazy.get());
                for code in 0u32..=0xFFFF {
                    // Require an authoritative ToUnicode entry. If the
                    // font has no ToUnicode CMap at all we conservatively
                    // skip injection — the fallback chain would only
                    // produce the misleading identity mapping.
                    let unicode_str =
                        match to_unicode_cmap.as_ref().and_then(|cmap| cmap.get(&code)) {
                            Some(s) if !s.is_empty() && s.as_ref() != "\u{FFFD}" => s.into_owned(),
                            _ => continue,
                        };
                    let cp = match unicode_str.chars().next() {
                        Some(c) => c as u32,
                        None => continue,
                    };
                    // Bare C0 controls (other than the legitimate
                    // whitespace handled in char_to_unicode) never name
                    // a real glyph — drop them so we don't inject a
                    // cmap entry that points U+0000..U+001F at random
                    // GIDs.
                    if matches!(cp, 0x00..=0x08 | 0x0B..=0x0C | 0x0E..=0x1F) {
                        continue;
                    }
                    // Only emit a Unicode→GID mapping when we have a
                    // real byte/CID → GID resolver from the source PDF.
                    // Falling back to identity for simple fonts whose
                    // CFF encoding parser couldn't extract a mapping
                    // produces a synthetic cmap that points Unicode at
                    // the wrong CFF charset positions: the round-trip
                    // emits Type0+Identity-H+CIDFontType0 and the
                    // viewer reads `glyph_at_charset[byte_code]`,
                    // which only equals the source glyph when CFF
                    // charset == StandardEncoding byte order — rarely
                    // true for subsetted CFF. Without a real mapping
                    // we leave the font un-patched, and office_oxide
                    // falls back to base-14 Helvetica via
                    // `EmbeddedFont::has_usable_unicode_cmap`.
                    let gid_opt = if let Some(ref map) = font_arc.cff_gid_map {
                        if code <= 0xFF {
                            map.get(&(code as u8)).copied()
                        } else {
                            None
                        }
                    } else if let Some(ref cid_map) = font_arc.cid_to_gid_map {
                        if code <= 0xFFFF {
                            Some(cid_map.get_gid(code as u16))
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    if let Some(gid) = gid_opt {
                        uni_to_gid.insert(cp, gid);
                    }
                }

                // Build GID → width from the source PDF's /W array.
                // For CIDFontType0+Identity-H: CID == GID directly.
                // For CIDFontType2: CID → GID via CIDToGIDMap.
                // For simple CFF (cff_gid_map): byte-code → GID.
                let mut gid_to_width: HashMap<u16, u16> = HashMap::new();
                if let Some(ref cid_widths) = font_arc.cid_widths {
                    if font_arc.cid_font_type.as_deref() == Some("CIDFontType0") {
                        for (&cid, &w) in cid_widths {
                            gid_to_width.insert(cid, w.round() as u16);
                        }
                    } else if let Some(ref cid_map) = font_arc.cid_to_gid_map {
                        for (&cid, &w) in cid_widths {
                            let gid = cid_map.get_gid(cid);
                            gid_to_width.insert(gid, w.round() as u16);
                        }
                    } else {
                        for (&cid, &w) in cid_widths {
                            gid_to_width.insert(cid, w.round() as u16);
                        }
                    }
                } else if let Some(ref cff_map) = font_arc.cff_gid_map {
                    // Simple CFF font: width-by-byte-code in font_arc.widths.
                    if let (Some(widths), Some(first)) =
                        (font_arc.widths.as_ref(), font_arc.first_char)
                    {
                        for (i, w) in widths.iter().enumerate() {
                            let byte = first + i as u32;
                            if byte > 0xFF {
                                break;
                            }
                            if let Some(&gid) = cff_map.get(&(byte as u8)) {
                                gid_to_width.insert(gid, w.round() as u16);
                            }
                        }
                    }
                }

                let entry = by_name
                    .entry(canonical.to_string())
                    .or_insert_with(|| (data.as_ref().clone(), HashMap::new(), HashMap::new()));
                // Same total-order choice as `extract_embedded_fonts`: largest
                // program, ties broken bytewise, rather than whichever HashMap
                // order surfaced first. (Size is a heuristic for the richer
                // subset, not a proof of superset - see the note there.)
                //
                // KNOWN GAP, deliberately left for a follow-up: the maps below
                // still merge across ALL subsets while the emitted program is now
                // a single chosen one, so a GID in the maps need not exist in the
                // program we hand back. Worse, when two subsets disagree about a
                // codepoint's GID - which subsets of one base font routinely do -
                // `or_insert` keeps whichever arrived first, so the maps carry the
                // very HashMap-order nondeterminism this fix removes from the
                // program. Fixing it means binding the maps to the chosen subset
                // instead of merging; that is a behaviour change (coverage may
                // shrink where subsets are disjoint) and belongs in its own PR.
                let cand = data.as_ref();
                if (cand.len(), cand.as_slice()) > (entry.0.len(), entry.0.as_slice()) {
                    entry.0 = cand.clone();
                }
                for (cp, gid) in uni_to_gid {
                    entry.1.entry(cp).or_insert(gid);
                }
                for (gid, w) in gid_to_width {
                    entry.2.entry(gid).or_insert(w);
                }
            }
        }

        let mut out: Vec<(String, Vec<u8>, HashMap<u32, u16>, HashMap<u16, u16>)> = by_name
            .into_iter()
            .map(|(name, (data, cmap, widths))| (name, data, cmap, widths))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }
}
