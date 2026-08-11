use super::parsing::*;
use super::preflight::*;
use super::*;

impl PdfDocument {
    /// Document-aware extension of `font_identity_hash_cheap` that folds the
    /// *content* of a font's document-specific streams — its `/ToUnicode` CMap
    /// and embedded font program(s) — plus the descendant CIDFont's width
    /// metrics (`/DW`, `/DW2`, `/W`, `/W2`) and stream-form `/CIDToGIDMap` into
    /// the identity hash.
    ///
    /// Why content, not just references: `font_identity_hash_cheap` folds only
    /// the *reference* (object id/gen) of `/ToUnicode`, and the global cache is
    /// skipped only for *canonical* subset fonts (`AAAAAA+`, six uppercase
    /// letters + `+`; see `is_subset_basefont`). A non-canonical subset tag
    /// such as `/CIDFont+F1` is therefore still shared cross-document, and
    /// PDFs emitted from a common template reuse the same `/ToUnicode` object
    /// number — so two genuinely different fonts that merely share a
    /// `/BaseFont` name produce an identical cheap hash. Keyed only by that
    /// hash, the cross-document global cache (Layer 6) served a later document
    /// the *earlier* font's parsed `FontInfo`, and its glyph→Unicode mapping
    /// came out as a constant-offset cipher or control/PUA junk (e.g.
    /// `SUMMARY` → `6800$5<`). Folding the `/ToUnicode` stream bytes — and the
    /// embedded `/FontFile{,2,3}` bytes — gives such fonts distinct keys so
    /// they can never collide regardless of subset-tag form or object reuse,
    /// while genuinely identical fonts still dedup. This completes the
    /// cross-document hardening from #595/#597/#598 (which folded the
    /// `/ToUnicode` *reference* and the `/Widths`, and excluded canonical
    /// `AAAAAA+` subsets), applied to the field that actually decodes text.
    ///
    /// Cost: a few extra `load_object` calls (the `/ToUnicode` stream, each
    /// descendant CIDFont, the `/FontDescriptor`s and their font programs) on
    /// the first encounter of a font per document; subsequent calls hit
    /// `font_id_hash_cache`, and the loads themselves are served from the
    /// object cache that `FontInfo::from_dict` populates anyway. Stream bytes
    /// are folded *raw* (still encoded) — see `fold_stream_bytes`.
    pub(super) fn font_identity_hash_with_descendants(&self, font_obj: &Object) -> u64 {
        use std::hash::{Hash, Hasher};
        // Seed with the cheap inline hash so existing identity coverage is
        // preserved bit-for-bit when there are no streams/descendants to fold.
        let base = Self::font_identity_hash_cheap(font_obj);
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        base.hash(&mut hasher);

        if let Some(d) = font_obj.as_dict() {
            // /ToUnicode stream BYTES — the decisive discriminator. The cheap
            // hash folds only this stream's reference; folding its content is
            // what stops same-named, differently-mapped fonts from colliding
            // across documents when the cheap key matches (#595).
            if let Some(to_unicode) = d.get("ToUnicode") {
                17u8.hash(&mut hasher);
                self.fold_stream_bytes(to_unicode, &mut hasher);
            }

            // /Encoding CONTENT for a referenced or inline encoding dictionary.
            // The cheap hash can only fold a constant marker for a
            // referenced/dict /Encoding (it does not resolve), so two simple
            // fonts that share a /BaseFont name but re-encode through DIFFERENT
            // /Differences arrays collide on the cheap key. That is common in
            // subsetted PDFs: several /BaseFont "Times-Roman" instances each
            // carry a per-instance, frequency-ordered /Encoding (code 1 -> first
            // glyph used, ...). With no /ToUnicode and no embedded program (a
            // non-embedded base font) NOTHING else distinguishes them, so the
            // per-document cache served the first font's parsed FontInfo for the
            // second, decoding its body text through the wrong /Differences - a
            // substitution-cipher scramble ("the" -> "bis"). Fold the resolved
            // base encoding name and the /Differences [code -> glyph name] pairs
            // so such fonts get distinct keys, while genuinely identical
            // encodings still dedup.
            if let Some(enc) = d.get("Encoding") {
                if let Some(enc_obj) = self.resolve_indirect_for_hash(enc) {
                    if let Some(enc_dict) = enc_obj.as_dict() {
                        20u8.hash(&mut hasher);
                        if let Some(Object::Name(base)) = enc_dict.get("BaseEncoding") {
                            base.hash(&mut hasher);
                        }
                        if let Some(diffs) = enc_dict.get("Differences") {
                            if let Some(diffs_obj) = self.resolve_indirect_for_hash(diffs) {
                                if let Some(arr) = diffs_obj.as_array() {
                                    for item in arr {
                                        match item {
                                            Object::Integer(i) => i.hash(&mut hasher),
                                            Object::Name(n) => n.hash(&mut hasher),
                                            Object::Reference(r) => {
                                                r.id.hash(&mut hasher);
                                                r.gen.hash(&mut hasher);
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Simple fonts (Type1/TrueType) carry their embedded program on the
            // top-level /FontDescriptor. Two subset fonts that share a
            // /BaseFont name but embed different glyph programs must not alias.
            if let Some(fd) = d.get("FontDescriptor") {
                if let Some(fd_obj) = self.resolve_indirect_for_hash(fd) {
                    self.fold_font_program(&fd_obj, 18, &mut hasher);
                }
            }

            if let Some(Object::Array(arr)) = d.get("DescendantFonts") {
                // Domain separator for the descendant section.
                11u8.hash(&mut hasher);
                for item in arr {
                    let resolved = match item {
                        Object::Reference(r) => self.load_object(*r).ok(),
                        Object::Dictionary(_) => Some(item.clone()),
                        _ => None,
                    };
                    let desc = match resolved {
                        Some(d) => d,
                        None => continue,
                    };
                    let dd = match desc.as_dict() {
                        Some(dd) => dd,
                        None => continue,
                    };

                    // /DW — default horizontal width on the CIDFont. Always
                    // int in well-formed PDFs; we accept Real defensively.
                    if let Some(dw) = dd.get("DW") {
                        12u8.hash(&mut hasher);
                        Self::hash_pdf_object_deterministic(dw, &mut hasher);
                    }
                    // /DW2 — default vertical metrics [v_y w1y]. Two-element
                    // numeric array per ISO 32000-1 §9.7.4.3.
                    if let Some(dw2) = dd.get("DW2") {
                        13u8.hash(&mut hasher);
                        Self::hash_pdf_object_deterministic(dw2, &mut hasher);
                    }
                    // /W — per-CID horizontal widths, may use form-a
                    // (c [w1 w2 …]) or form-b (c_first c_last w).
                    if let Some(w) = dd.get("W") {
                        14u8.hash(&mut hasher);
                        Self::hash_pdf_object_deterministic(w, &mut hasher);
                    }
                    // /W2 — per-CID vertical metrics, analogous to /W.
                    if let Some(w2) = dd.get("W2") {
                        15u8.hash(&mut hasher);
                        Self::hash_pdf_object_deterministic(w2, &mut hasher);
                    }
                    // /CIDSystemInfo — folded so otherwise-identical dicts
                    // targeting different registries don't collide.
                    if let Some(csi) = dd.get("CIDSystemInfo") {
                        16u8.hash(&mut hasher);
                        Self::hash_pdf_object_deterministic(csi, &mut hasher);
                    }
                    // Descendant /Subtype: CIDFontType0 (CFF) and CIDFontType2
                    // (TrueType) are not interchangeable even with identical
                    // name + metrics; the top-level Subtype is `Type0` for both.
                    if let Some(st) = dd.get("Subtype") {
                        19u8.hash(&mut hasher);
                        Self::hash_pdf_object_deterministic(st, &mut hasher);
                    }
                    // Embedded CIDFont program lives on the descendant's
                    // /FontDescriptor (/FontFile2 for TrueType, /FontFile3 for
                    // CFF). Folded under a distinct section so it cannot alias
                    // a simple font's top-level program.
                    if let Some(fd) = dd.get("FontDescriptor") {
                        if let Some(fd_obj) = self.resolve_indirect_for_hash(fd) {
                            self.fold_font_program(&fd_obj, 20, &mut hasher);
                        }
                    }
                    // Descendant /CIDToGIDMap: the *stream* form remaps
                    // CID→glyph (§9.7.4.3), so two otherwise-identical embedded
                    // CIDFontType2 fonts with different maps select different
                    // glyphs and must not alias. The `/Identity` name — and an
                    // absent entry, which defaults to Identity — fold nothing,
                    // so the common path's key is unchanged (and an explicit
                    // `/Identity` still dedups with an absent one).
                    if let Some(c2g) = dd.get("CIDToGIDMap") {
                        if !matches!(c2g, Object::Name(_)) {
                            21u8.hash(&mut hasher);
                            self.fold_stream_bytes(c2g, &mut hasher);
                        }
                    }
                }
            }
        }

        hasher.finish()
    }

    /// Resolve a single level of indirection for hashing: returns the
    /// referenced object, the object itself when already inline, or `None`
    /// when a reference cannot be loaded (cycle/missing). Used only to reach a
    /// `/FontDescriptor` dict — it never re-enters the font dict, so it cannot
    /// loop.
    pub(super) fn resolve_indirect_for_hash(&self, obj: &Object) -> Option<Object> {
        match obj {
            Object::Reference(r) => self.load_object(*r).ok(),
            other => Some(other.clone()),
        }
    }

    /// Fold the *raw* bytes of a (possibly indirectly-referenced) stream into
    /// the hash. Folds nothing when the object is absent, unreadable, or not a
    /// stream.
    ///
    /// Raw — still-encoded — bytes are deliberate. They are a sufficient
    /// discriminator: different decoded content yields different encoded bytes
    /// under any deterministic filter, so this never produces a *false* dedup
    /// (two different fonts sharing a key). It avoids inflating large font
    /// programs on the cache-key path. The only cost is a *missed* dedup when
    /// the same logical content is stored under two different filters
    /// (e.g. raw vs. FlateDecode) — harmless, and not a pattern a single
    /// producer emits within a corpus.
    pub(super) fn fold_stream_bytes<H: std::hash::Hasher>(&self, obj: &Object, hasher: &mut H) {
        use std::hash::Hash;
        let owned;
        let stream: &Object = match obj {
            Object::Stream { .. } => obj,
            Object::Reference(r) => match self.load_object(*r) {
                Ok(o) => {
                    owned = o;
                    &owned
                }
                Err(_) => return,
            },
            _ => return,
        };
        if let Object::Stream { data, .. } = stream {
            (data.len() as u64).hash(hasher);
            data.as_ref().hash(hasher);
        }
    }

    /// Fold any embedded font program (`/FontFile`, `/FontFile2`,
    /// `/FontFile3`) reachable from a `/FontDescriptor` dict into the hash,
    /// namespaced by `section` so a simple font's program and a descendant
    /// CIDFont's program cannot alias each other.
    pub(super) fn fold_font_program<H: std::hash::Hasher>(
        &self,
        descriptor: &Object,
        section: u8,
        hasher: &mut H,
    ) {
        use std::hash::Hash;
        let dict = match descriptor.as_dict() {
            Some(d) => d,
            None => return,
        };
        for (variant, key) in ["FontFile", "FontFile2", "FontFile3"].iter().enumerate() {
            if let Some(ff) = dict.get(*key) {
                section.hash(hasher);
                (variant as u8).hash(hasher);
                self.fold_stream_bytes(ff, hasher);
            }
        }
    }

    /// Hash a PDF `Object` deterministically. Used by the descendant-aware
    /// font identity hash to fold raw width-array content into the key.
    ///
    /// Cycles are not possible for /W, /W2, /DW2 or /CIDSystemInfo content
    /// in any conformant PDF: these are pure data subtrees (numbers,
    /// arrays of numbers, occasional name/integer dicts), never indirect
    /// references back to a font dict. We still avoid recursing into
    /// streams (whose data we deliberately exclude from the cheap hash)
    /// and into unresolved references (we hash the ref's id/gen, not the
    /// pointed-to bytes — the per-font cache key already covers the
    /// referenced descendant CIDFont).
    pub(super) fn hash_pdf_object_deterministic<H: std::hash::Hasher>(
        obj: &Object,
        hasher: &mut H,
    ) {
        use std::hash::Hash;
        match obj {
            Object::Null => 0u8.hash(hasher),
            Object::Boolean(b) => {
                1u8.hash(hasher);
                b.hash(hasher);
            }
            Object::Integer(i) => {
                2u8.hash(hasher);
                i.hash(hasher);
            }
            // Bit-pattern hash so two equal values hash identically without
            // tripping over f64's missing `Hash` impl. NaN is not produced
            // by PDF parsers from numeric tokens.
            Object::Real(r) => {
                3u8.hash(hasher);
                r.to_bits().hash(hasher);
            }
            Object::String(s) => {
                4u8.hash(hasher);
                s.hash(hasher);
            }
            Object::Name(n) => {
                5u8.hash(hasher);
                n.hash(hasher);
            }
            Object::Array(arr) => {
                6u8.hash(hasher);
                (arr.len() as u64).hash(hasher);
                for item in arr {
                    Self::hash_pdf_object_deterministic(item, hasher);
                }
            }
            Object::Dictionary(d) => {
                7u8.hash(hasher);
                // Sort keys for deterministic ordering — HashMap iteration
                // is randomized per process.
                let mut keys: Vec<&str> = d.keys().map(|k| k.as_str()).collect();
                keys.sort_unstable();
                (keys.len() as u64).hash(hasher);
                for k in keys {
                    k.hash(hasher);
                    if let Some(v) = d.get(k) {
                        Self::hash_pdf_object_deterministic(v, hasher);
                    }
                }
            }
            Object::Reference(r) => {
                8u8.hash(hasher);
                r.id.hash(hasher);
                r.gen.hash(hasher);
            }
            // Streams: dict shape only; we do not pull stream data into
            // the font identity hash (kept consistent with the cheap path).
            Object::Stream { dict, .. } => {
                9u8.hash(hasher);
                let mut keys: Vec<&str> = dict.keys().map(|k| k.as_str()).collect();
                keys.sort_unstable();
                (keys.len() as u64).hash(hasher);
                for k in keys {
                    k.hash(hasher);
                    if let Some(v) = dict.get(k) {
                        Self::hash_pdf_object_deterministic(v, hasher);
                    }
                }
            }
        }
    }

    pub(super) fn font_identity_hash_cheap(font_obj: &Object) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();

        if let Some(d) = font_obj.as_dict() {
            // BaseFont: primary identity — unique per font within a document
            if let Some(Object::Name(n)) = d.get("BaseFont") {
                1u8.hash(&mut hasher);
                n.hash(&mut hasher);
            }
            // Subtype: Type1, TrueType, Type0, CIDFontType0, CIDFontType2
            if let Some(Object::Name(n)) = d.get("Subtype") {
                2u8.hash(&mut hasher);
                n.hash(&mut hasher);
            }
            // Encoding: hash inline name or presence of reference
            if let Some(enc) = d.get("Encoding") {
                3u8.hash(&mut hasher);
                match enc {
                    Object::Name(n) => n.hash(&mut hasher),
                    Object::Reference(_) => b"enc_ref".hash(&mut hasher),
                    Object::Dictionary(_) => b"enc_dict".hash(&mut hasher),
                    _ => {}
                }
            }
            // ToUnicode: hash content via reference or inline presence
            if let Some(to_unicode) = d.get("ToUnicode") {
                4u8.hash(&mut hasher);
                if let Some(r) = to_unicode.as_reference() {
                    r.id.hash(&mut hasher);
                    r.gen.hash(&mut hasher);
                }
            }
            // FontDescriptor: hash presence
            if d.get("FontDescriptor").is_some() {
                5u8.hash(&mut hasher);
            }
            // DescendantFonts: hash references for Type0 fonts
            if let Some(Object::Array(arr)) = d.get("DescendantFonts") {
                6u8.hash(&mut hasher);
                for item in arr {
                    if let Some(r) = item.as_reference() {
                        r.id.hash(&mut hasher);
                        r.gen.hash(&mut hasher);
                    }
                }
            }
            // #598: width metrics. Two non-subset fonts can share
            // BaseFont + Subtype + Encoding yet ship different glyph widths —
            // Standard-14 fonts may carry producer-specific /Widths overrides
            // (§9.6.2.2), and differently-optimized embeds of the same named
            // font diverge similarly. Without folding widths into the key,
            // such fonts collide on the cross-document cache and the second
            // document gets the first's advances. We hash the simple-font
            // char range + width table and the Type0 default width. Only
            // values present inline on this dict are reachable (this is a pure
            // function over the font object); a referenced /Widths or the
            // descendant CIDFont /W array falls back to the coarser key — an
            // accepted, documented limitation, not a new regression.
            if let Some(Object::Integer(first_char)) = d.get("FirstChar") {
                7u8.hash(&mut hasher);
                first_char.hash(&mut hasher);
            }
            if let Some(Object::Integer(last_char)) = d.get("LastChar") {
                8u8.hash(&mut hasher);
                last_char.hash(&mut hasher);
            }
            if let Some(Object::Array(widths)) = d.get("Widths") {
                9u8.hash(&mut hasher);
                (widths.len() as u64).hash(&mut hasher);
                for w in widths {
                    match w {
                        Object::Integer(i) => i.hash(&mut hasher),
                        // Bit-pattern hash so equal widths hash equally
                        // (these are glyph advances, never NaN in practice).
                        Object::Real(r) => r.to_bits().hash(&mut hasher),
                        _ => 0u8.hash(&mut hasher),
                    }
                }
            }
            // Type0 default width, when present inline on the font dict.
            if let Some(Object::Integer(dw)) = d.get("DW") {
                10u8.hash(&mut hasher);
                dw.hash(&mut hasher);
            }
        }
        hasher.finish()
    }

    /// Whether a font dictionary describes a font that is *document-local* and
    /// therefore must never be served from / inserted into the cross-document
    /// global font cache (Layer 6), even if its cheap identity hash collides
    /// with a font in another document.
    ///
    /// Type 3 fonts (PDF 32000-1 §9.6.5) define their glyphs as streams of PDF
    /// graphics operators in a `/CharProcs` dictionary whose procedures
    /// reference the *owning document's* resources (XObjects, ColorSpaces,
    /// ExtGState, …). Two Type 3 fonts from different documents that happen to
    /// share `/Name` + `/Encoding` shape are NOT interchangeable: serving one
    /// document's parsed `FontInfo` for the other yields wrong glyphs. Such
    /// fonts carry no subset prefix, so the cheap hash cannot distinguish them
    /// — this predicate gates them out of the global cache instead (#597).
    pub(super) fn font_is_document_local(font_obj: &Object) -> bool {
        let dict = match font_obj.as_dict() {
            Some(d) => d,
            None => return false,
        };

        // Type 3 fonts reference this document's resources via their CharProcs,
        // so a cached FontInfo cannot cross PdfDocument boundaries.
        if dict.get("Subtype").and_then(|s| s.as_name()) == Some("Type3") {
            return true;
        }

        // Subset fonts carry a document-specific glyph subset and ToUnicode
        // CMap, so they are unsafe to share across documents even when the
        // BaseFont name collides. A subset BaseFont is tagged with exactly six
        // uppercase letters and a '+' per ISO 32000-1:2008 §9.6.4
        // (e.g. `AAAAAA+ArialUnicodeMS`).
        match dict.get("BaseFont").and_then(|b| b.as_name()) {
            Some(base_font) => Self::is_subset_basefont(base_font),
            // A non-Type3 font is required by the spec to carry /BaseFont; if it
            // is absent we cannot prove the font is shareable, so fail safe and
            // treat it as document-local rather than risk poisoning the cache.
            None => true,
        }
    }

    /// Detect a PDF subset-font tag on a `/BaseFont` name: exactly six uppercase
    /// ASCII letters followed by `+`, per ISO 32000-1:2008 §9.6.4 (e.g.
    /// `AAAAAA+ArialUnicodeMS`). `is_ascii_uppercase` is precisely A–Z, so
    /// multibyte (CJK) names never satisfy the test and are treated as full
    /// fonts — correct, since subset tags are by definition ASCII A–Z.
    pub(super) fn is_subset_basefont(base_font: &str) -> bool {
        let bytes = base_font.as_bytes();
        bytes.len() > 7 && bytes[6] == b'+' && bytes[..6].iter().all(|b| b.is_ascii_uppercase())
    }
}
