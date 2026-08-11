use super::*;

pub(super) fn classify_embedded_font(data: &Arc<Vec<u8>>) -> (bool, bool) {
    (|| {
        let face = ttf_parser::Face::parse(data, 0).ok()?;
        let cmap = face.tables().cmap?;
        let mut saw_byte_indexed = false;
        let mut saw_unicode = false;
        for sub in cmap.subtables {
            use ttf_parser::PlatformId;
            match sub.platform_id {
                PlatformId::Unicode => saw_unicode = true,
                PlatformId::Windows if sub.encoding_id == 1 || sub.encoding_id == 10 => {
                    saw_unicode = true;
                }
                PlatformId::Macintosh if sub.encoding_id == 0 => saw_byte_indexed = true,
                _ => {}
            }
        }
        Some((saw_byte_indexed && !saw_unicode, saw_unicode))
    })()
    .unwrap_or((false, false))
}

/// Resolve a single PDF content byte to a GID by consulting the font's
/// own cmap subtables. Prefers a byte-indexed (Macintosh Roman) subtable
/// when present; falls back to ttf-parser's default Unicode resolution
/// for ASCII-range bytes if no byte-indexed subtable exists.
pub(super) fn cmap_byte_to_gid(face: &ttf_parser::Face, byte: u8) -> Option<u16> {
    if let Some(cmap) = face.tables().cmap {
        for sub in cmap.subtables {
            use ttf_parser::PlatformId;
            if matches!(sub.platform_id, PlatformId::Macintosh) && sub.encoding_id == 0 {
                if let Some(gid) = sub.glyph_index(byte as u32) {
                    return Some(gid.0);
                }
            }
        }
    }
    face.glyph_index(byte as char).map(|g| g.0)
}

/// Process-wide cache for the system font database.
///
/// `fontdb::Database::load_system_fonts()` walks every font directory on
/// the host and parses each face it finds, which typically takes several
/// seconds on first call. Before this cache was introduced, every
/// `TextRasterizer::new()` (and therefore every `PageRenderer::new()`)
/// paid that cost, and callers who constructed a fresh `PageRenderer`
/// per page — which is the obvious first-draft usage from the Python /
/// CLI surface — hit the scan once per page. A cold-cache ORAFOL 5400
/// render took ~4.1 s on a warm machine for a single page because of
/// this. See issue #331.
///
/// Switching to a process-wide `OnceLock<Arc<fontdb::Database>>` loads
/// the database exactly once per process, and every subsequent
/// `TextRasterizer` constructor takes a cheap `Arc::clone`. Wrapping
/// in `Arc` is important so that the cache is still cheaply shareable
/// across `TextRasterizer` instances in different rendering contexts
/// without re-copying the full parsed font metadata. Callers that want
/// a private / modified database can still construct one by hand and
/// bypass this cache via `TextRasterizer::with_fontdb()`.
static SYSTEM_FONTDB: std::sync::OnceLock<std::sync::Arc<fontdb::Database>> =
    std::sync::OnceLock::new();

pub(super) fn system_fontdb() -> std::sync::Arc<fontdb::Database> {
    SYSTEM_FONTDB
        .get_or_init(|| {
            // Scanning system fonts is the most expensive setup on the render path, and
            // the text path must never pay for it. The counter makes "text mode does not
            // start the renderer" a measured fact rather than a claim about call graphs.
            crate::metrics::record_font_database_load();
            let mut db = fontdb::Database::new();
            db.load_system_fonts();
            std::sync::Arc::new(db)
        })
        .clone()
}

/// Process-wide cache mapping fontdb::ID → (font bytes, face index).
///
/// Without this cache, `load_font_data` calls `with_face_data(...to_vec())`
/// which clones the entire font binary (often 300–500 KB for Liberation Serif
/// or Times New Roman) on every `render_text` call. A two-page text PDF can
/// trigger hundreds of such clones per render pass. This cache reduces each
/// subsequent access to a cheap `Arc::clone`.
static FONT_BYTES_CACHE: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<fontdb::ID, (Arc<Vec<u8>>, u32)>>,
> = std::sync::OnceLock::new();

pub(super) fn cached_font_bytes(
    id: fontdb::ID,
    db: &fontdb::Database,
) -> Option<(Arc<Vec<u8>>, u32)> {
    let cache =
        FONT_BYTES_CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    {
        let guard = cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = guard.get(&id) {
            return Some(entry.clone());
        }
    }
    let mut result: Option<(Arc<Vec<u8>>, u32)> = None;
    db.with_face_data(id, |data, index| {
        result = Some((Arc::new(data.to_vec()), index));
    });
    if let Some(ref entry) = result {
        let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
        guard.insert(id, entry.clone());
    }
    result
}

/// Process-wide CJK fallback font — loaded once per process, shared by all
/// TextRasterizer instances.
///
/// Before this cache, every glyph that fell through to the CJK path called
/// `load_cjk_fallback()`, which iterated 7+ fontdb queries and cloned a
/// 10–20 MB Noto CJK binary. Now that work is done exactly once.
static CJK_FALLBACK: std::sync::OnceLock<Option<(fontdb::ID, Arc<Vec<u8>>, u32)>> =
    std::sync::OnceLock::new();

pub(super) fn get_cjk_fallback_cached(
    db: &fontdb::Database,
) -> Option<(fontdb::ID, Arc<Vec<u8>>, u32)> {
    CJK_FALLBACK
        .get_or_init(|| {
            let prioritized_variants = [
                "Noto Sans CJK SC",
                "Noto Serif CJK SC",
                "Droid Sans Fallback",
                "SimSun",
                "WenQuanYi Micro Hei",
                "Noto Sans CJK JP",
                "Noto Serif CJK JP",
            ];
            for variant in prioritized_variants {
                let query = fontdb::Query {
                    families: &[fontdb::Family::Name(variant)],
                    weight: fontdb::Weight::NORMAL,
                    stretch: fontdb::Stretch::Normal,
                    style: fontdb::Style::Normal,
                };
                if let Some(id) = db.query(&query) {
                    if let Some((arc, idx)) = cached_font_bytes(id, db) {
                        log::debug!(
                            "CJK fallback: matched '{}', idx={}, size={} bytes",
                            variant,
                            idx,
                            arc.len()
                        );
                        return Some((id, arc, idx));
                    }
                }
            }
            let query = fontdb::Query {
                families: &[fontdb::Family::SansSerif],
                weight: fontdb::Weight::NORMAL,
                stretch: fontdb::Stretch::Normal,
                style: fontdb::Style::Normal,
            };
            if let Some(id) = db.query(&query) {
                if let Some((arc, idx)) = cached_font_bytes(id, db) {
                    return Some((id, arc, idx));
                }
            }
            None
        })
        .as_ref()
        .map(|(id, arc, idx)| (*id, Arc::clone(arc), *idx))
}

impl TextRasterizer {
    /// Get font info for a specific font name from resources.
    #[allow(dead_code)]
    pub(super) fn get_font_info(
        &self,
        doc: &PdfDocument,
        resources: &Object,
        font_name: &str,
    ) -> Result<crate::fonts::FontInfo> {
        if let Object::Dictionary(res_dict) = resources {
            if let Some(Object::Dictionary(fonts)) = res_dict.get("Font") {
                if let Some(font_ref) = fonts.get(font_name) {
                    let font_obj = doc.resolve_object(font_ref)?;
                    let info = crate::fonts::FontInfo::from_dict(&font_obj, doc)?;
                    log::debug!("Resolved font '{}': subtype={}, encoding={:?}, has_to_unicode={}, has_embedded={}",
                        info.base_font, info.subtype, info.encoding, info.to_unicode.is_some(), info.embedded_font_data.is_some());
                    return Ok(info);
                }
            }
        }
        Err(Error::InvalidPdf(format!("Font {} not found", font_name)))
    }

    /// Find and load font data from system. Returns a `fontdb::ID` alongside
    /// the `Arc`-wrapped bytes so callers can look up the parsed-face cache.
    pub(super) fn load_font_data(
        &self,
        pdf_font_name: &str,
    ) -> Option<(fontdb::ID, Arc<Vec<u8>>, u32)> {
        // Strip subset prefix (e.g., "ABCDEF+FontName" -> "FontName")
        let clean_name = if let Some(plus_idx) = pdf_font_name.find('+') {
            &pdf_font_name[plus_idx + 1..]
        } else {
            pdf_font_name
        };

        // Handle common CJK names and encoding markers
        let is_cjk_probability = clean_name.contains("GB2312")
            || clean_name.contains("Identity")
            || clean_name.contains("楷体")
            || clean_name.contains("æ¥·ä½") // Mojibake variant
            || clean_name.contains("宋体")
            || clean_name.contains("å®\u{008b}ä½") // Mojibake variant
            || clean_name.contains("黑体")
            || clean_name.contains("é»\u{0091}ä½") // Mojibake variant
            || clean_name.contains("FangSong")
            || clean_name.contains("SimSun")
            || clean_name.contains("SimHei")
            || clean_name.contains("KaiTi")
            || pdf_font_name == "F1";

        let final_name = if clean_name.contains("楷体")
            || clean_name.contains("æ¥·ä½")
            || clean_name.contains("KaiTi")
        {
            "KaiTi"
        } else if clean_name.contains("宋体")
            || clean_name.contains("å®\u{008b}ä½")
            || clean_name.contains("SimSun")
        {
            "SimSun"
        } else if clean_name.contains("黑体")
            || clean_name.contains("é»\u{0091}ä½")
            || clean_name.contains("SimHei")
        {
            "SimHei"
        } else {
            clean_name
        };

        // Map well-known PDF/LaTeX font names to system font equivalents
        let mut variants = vec![final_name.to_string()];

        // URW/TeX font mappings to URW base35 system fonts
        if clean_name.contains("URWPalladioL") || clean_name.contains("Palatino") {
            variants.insert(0, "P052".to_string());
            variants.push("Palatino Linotype".to_string());
            variants.push("TeX Gyre Pagella".to_string());
        } else if clean_name.contains("NimbusRomNo9L") || clean_name.contains("NimbusRoman") {
            variants.insert(0, "Nimbus Roman".to_string());
            variants.push("Times New Roman".to_string());
        } else if clean_name.contains("NimbusSanL") || clean_name.contains("NimbusSans") {
            variants.insert(0, "Nimbus Sans".to_string());
            variants.push("Arial".to_string());
        } else if clean_name.contains("NimbusMonL") || clean_name.contains("NimbusMono") {
            variants.insert(0, "Nimbus Mono PS".to_string());
            variants.push("Courier New".to_string());
        } else if clean_name.contains("CMSS")
            || clean_name.contains("CMR")
            || clean_name.contains("CMBX")
        {
            // Computer Modern fonts (LaTeX) — use Latin Modern or serif fallback
            variants.push("Latin Modern Roman".to_string());
            variants.push("Computer Modern".to_string());
        } else if clean_name.contains("URWBookmanL") || clean_name.contains("Bookman") {
            variants.insert(0, "Bookman URW".to_string());
        } else if clean_name.contains("CenturySchL") || clean_name.contains("NewCentury") {
            variants.insert(0, "C059".to_string());
        } else if clean_name.contains("URWChanceryL") || clean_name.contains("Chancery") {
            variants.insert(0, "Z003".to_string());
        }

        if is_cjk_probability {
            variants.push("Noto Sans CJK SC".to_string());
            variants.push("Noto Serif CJK SC".to_string());
            variants.push("WenQuanYi Micro Hei".to_string());
            variants.push("Droid Sans Fallback".to_string());
        }

        // Generic fallbacks — detect serif vs sans-serif
        let is_serif = clean_name.contains("Roman")
            || clean_name.contains("Serif")
            || clean_name.contains("Times")
            || clean_name.contains("Palladio")
            || clean_name.contains("Palatino")
            || clean_name.contains("Bookman")
            || clean_name.contains("Garamond")
            || clean_name.contains("Century")
            || clean_name.contains("Georgia")
            || clean_name.contains("CMR")
            || clean_name.contains("CMBX")
            || clean_name.contains("CMTI");
        if is_serif {
            variants.push("Times New Roman".to_string());
            variants.push("Liberation Serif".to_string());
            variants.push("DejaVu Serif".to_string());
        }
        variants.push("Arial".to_string());
        variants.push("Helvetica".to_string());
        variants.push("Liberation Sans".to_string());
        variants.push("DejaVu Sans".to_string());
        variants.push("Noto Sans".to_string());
        variants.push("FreeSans".to_string());

        let weight = if pdf_font_name.contains("Bold") || pdf_font_name.contains("Black") {
            fontdb::Weight::BOLD
        } else {
            fontdb::Weight::NORMAL
        };

        let style = if pdf_font_name.contains("Italic") || pdf_font_name.contains("Oblique") {
            fontdb::Style::Italic
        } else {
            fontdb::Style::Normal
        };

        for variant in variants {
            let families = [
                fontdb::Family::Name(&variant),
                fontdb::Family::Serif,
                fontdb::Family::SansSerif,
            ];
            let query = fontdb::Query {
                families: &families,
                weight,
                stretch: fontdb::Stretch::Normal,
                style,
            };

            if let Some(id) = self.font_db().query(&query) {
                if let Some((arc_data, index)) = cached_font_bytes(id, self.font_db()) {
                    log::debug!(
                        "Matched system font for {}: variant={}, index={}, size={} bytes",
                        pdf_font_name,
                        variant,
                        index,
                        arc_data.len()
                    );
                    return Some((id, arc_data, index));
                }
            }
        }
        log::debug!(
            "No system font matched for '{}' after trying all fallback variants",
            pdf_font_name
        );
        None
    }

    /// Access the font database.
    pub(super) fn font_db(&self) -> &fontdb::Database {
        &self.fontdb
    }
}
