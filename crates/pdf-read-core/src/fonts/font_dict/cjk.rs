use super::*;

/// Decode a raw CJK multi-byte character code to Unicode using legacy encodings.
///
/// For Type0 fonts using named CJK CMaps (e.g., "GBK-EUC-H", "GB-EUC-H",
/// "ETen-B5-H", "EUC-H", "KSC-EUC-H"), the 2-byte value read from the content
/// stream is NOT an Adobe CID — it is a raw multi-byte encoding value (GBK,
/// EUC-CN, Big5, EUC-JP, or EUC-KR). Adobe-GB1 CIDs cap at ~30 553, so
/// `lookup_predefined_cmap` always returns None for GBK values ≥ 0xA1A1,
/// the caller falls through to a broken `char::from_u32` path that maps them
/// to Korean Hangul (same code-point range).
///
/// This function catches that case and decodes with encoding_rs so the correct
/// CJK characters come out.
pub(super) fn decode_cjk_raw_charcode(
    char_code: u32,
    enc_name: &str,
    cid_system_info: &Option<CIDSystemInfo>,
) -> Option<String> {
    let ordering = cid_system_info
        .as_ref()
        .map(|i| i.ordering.as_str())
        .unwrap_or("");

    // CORPUS-3: the bare Adobe predefined CMaps "H"/"V" are (overwhelmingly)
    // Adobe-Japan1-H/V and carry JIS X 0208 codes in GL form (both bytes
    // 0x21–0x7E). encoding_rs decodes EUC-JP (high bit set), so lift GL→EUC by
    // OR-ing 0x8080, then decode. Recovers non-embedded Japanese (noembed-jis7:
    // "あいうえお" was emitted as garbage "CACCCECGCI").
    if (enc_name == "H" || enc_name == "V") && (ordering == "Japan1" || ordering.is_empty()) {
        let hi = (char_code >> 8) & 0xFF;
        let lo = char_code & 0xFF;
        if (0x21..=0x7E).contains(&hi) && (0x21..=0x7E).contains(&lo) {
            let euc = [(hi | 0x80) as u8, (lo | 0x80) as u8];
            let (decoded, _, errors) = encoding_rs::EUC_JP.decode(&euc);
            if !errors {
                let r = decoded.replace('\u{FFFD}', "");
                if !r.is_empty() {
                    return Some(r);
                }
            }
        }
        // ASCII range (single-byte-ish codes 0x20–0x7E) pass through as-is.
        if char_code <= 0x7E {
            if let Some(c) = char::from_u32(char_code) {
                return Some(c.to_string());
            }
        }
    }

    // Determine which legacy encoding applies based on the CMap name and ordering.
    // CMap names that imply raw legacy encoding (not CID-keyed identity):
    let enc: Option<&'static encoding_rs::Encoding> = if enc_name.contains("GBK")
        || enc_name.contains("GB-")
        || enc_name.contains("GBpc")
        || (enc_name.contains("EUC") && (ordering == "GB1" || enc_name.starts_with("GB")))
    {
        Some(encoding_rs::GBK)
    } else if enc_name.contains("B5")
        || enc_name.contains("CNS")
        || (enc_name.contains("EUC") && ordering == "CNS1")
    {
        Some(encoding_rs::BIG5)
    } else if enc_name.contains("EUC") && ordering == "Japan1" {
        Some(encoding_rs::EUC_JP)
    } else if (enc_name.contains("KSC") || enc_name.contains("KSCms")) && ordering == "Korea1" {
        Some(encoding_rs::EUC_KR)
    } else {
        None
    };

    let enc = enc?;

    // Reconstruct the raw bytes from the 2-byte char_code (big-endian)
    let bytes: [u8; 2] = [((char_code >> 8) & 0xFF) as u8, (char_code & 0xFF) as u8];

    let (decoded, _, errors) = enc.decode(&bytes);
    if errors {
        return None;
    }
    // Skip the replacement character U+FFFD (decoding failed)
    let result = decoded.replace('\u{FFFD}', "");
    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}

// Maximum valid CID for each Adobe character collection (Fix C – OOB guard).
// CIDs beyond these values have no defined Unicode mapping; return None early
// to avoid accidental wrap-around in future table expansions.
//
// Sources:
//   Adobe-GB1-5 (TN #5079): 30,283 CIDs (0–30,283)
//   Adobe-Japan1-7 (TN #5078): 23,059 CIDs (0–23,059)
//   Adobe-CNS1-7 (TN #5080): 20,316 CIDs (0–20,316)
//   Adobe-Korea1-2 (TN #5093): 18,351 CIDs (0–18,351)
const CID_MAX_GB1: u16 = 30_283;
const CID_MAX_JAPAN1: u16 = 23_059;
const CID_MAX_CNS1: u16 = 20_316;
const CID_MAX_KOREA1: u16 = 18_351;

/// Lookup Unicode code point for a CID in a predefined Unicode-based CMap.
///
/// Predefined CMaps for CJK fonts map CID values from Adobe character collections to Unicode.
/// Per PDF Spec ISO 32000-1:2008 Section 9.7.5.2.
///
/// # Arguments
///
/// * `cmap_name` - The predefined CMap name (e.g., "UniGB-UCS2-H")
/// * `cid_system_info` - The CIDSystemInfo identifying the character collection
/// * `cid` - The Character ID (CID) to look up
///
/// # Returns
///
/// The corresponding Unicode code point, or None if not found.
///
/// # Predefined CMaps Supported
///
/// - UniGB-UCS2-H: Adobe-GB1 (Simplified Chinese)
/// - UniJIS-UCS2-H: Adobe-Japan1 (Japanese)
/// - UniCNS-UCS2-H: Adobe-CNS1 (Traditional Chinese)
/// - UniKS-UCS2-H: Adobe-Korea1 (Korean)
pub(super) fn lookup_predefined_cmap(
    cmap_name: &str,
    cid_system_info: &Option<CIDSystemInfo>,
    cid: u16,
) -> Option<u32> {
    // Verify that we have CIDSystemInfo to match against the CMap
    let system_info = cid_system_info.as_ref()?;

    // Fix C: guard out-of-bounds CIDs before hitting the lookup table.
    // CIDs beyond the collection maximum have no defined Unicode mapping.
    let max_cid = match system_info.ordering.as_str() {
        "GB1" => CID_MAX_GB1,
        "Japan1" => CID_MAX_JAPAN1,
        "CNS1" => CID_MAX_CNS1,
        "Korea1" => CID_MAX_KOREA1,
        // Adobe-Arabic-1 / Adobe-Persian-1: `lookup_adobe_arabic` rejects
        // unmapped CIDs itself, so the bound is just an early-out.
        "Arabic" | "Persian" => u16::MAX,
        _ => return None,
    };
    if cid > max_cid {
        log::debug!(
            "CID {} exceeds max {} for ordering '{}' → returning None (OOB)",
            cid,
            max_cid,
            system_info.ordering
        );
        return None;
    }

    // Route to the appropriate CMap lookup based on name and character collection
    match (cmap_name, system_info.ordering.as_str()) {
        ("UniGB-UCS2-H", "GB1") => lookup_adobe_gb1_to_unicode(cid),
        ("UniJIS-UCS2-H", "Japan1") => lookup_adobe_japan1_to_unicode(cid),
        ("UniCNS-UCS2-H", "CNS1") => lookup_adobe_cns1_to_unicode(cid),
        ("UniKS-UCS2-H", "Korea1") => lookup_adobe_korea1_to_unicode(cid),
        // Fallback: match by CIDSystemInfo ordering alone.
        // Some PDFs use encoding CMaps with custom names (e.g., "Adobe-Japan1-2")
        // that are identity mappings (charcode == CID). The CID→Unicode lookup
        // should still work based on the character collection ordering.
        (_, "GB1") => lookup_adobe_gb1_to_unicode(cid),
        (_, "Japan1") => lookup_adobe_japan1_to_unicode(cid),
        (_, "CNS1") => lookup_adobe_cns1_to_unicode(cid),
        (_, "Korea1") => lookup_adobe_korea1_to_unicode(cid),
        // Adobe-Arabic-1 / Adobe-Persian-1 CIDFonts without /ToUnicode (Nazanin,
        // Yagut, Mitra, Lotus). `lookup_adobe_arabic` is the §9.10.3 step-3
        // identity fallback; without it these decode as Latin-Extended-B garbage.
        (_, "Arabic") | (_, "Persian") => crate::fonts::cid_mappings::lookup_adobe_arabic(cid),
        _ => None,
    }
}

/// Map CID from Adobe-GB1 character collection to Unicode.
///
/// Adobe-GB1 contains Simplified Chinese characters from GB 2312 and extensions.
/// Reference: Adobe Technical Note #5079 (Adobe-GB1-4 Character Collection)
pub(super) fn lookup_adobe_gb1_to_unicode(cid: u16) -> Option<u32> {
    crate::fonts::cid_mappings::lookup_adobe_gb1(cid)
}

/// Map CID from Adobe-Japan1 character collection to Unicode.
///
/// Adobe-Japan1 contains Japanese characters from JIS X 0208, JIS X 0212, etc.
/// Reference: Adobe Technical Note #5078 (Adobe-Japan1-4 Character Collection)
pub(super) fn lookup_adobe_japan1_to_unicode(cid: u16) -> Option<u32> {
    crate::fonts::cid_mappings::lookup_adobe_japan1(cid)
}

/// Map CID from Adobe-CNS1 character collection to Unicode.
///
/// Adobe-CNS1 contains Traditional Chinese characters from CNS 11643 and extensions.
/// Reference: Adobe Technical Note #5080 (Adobe-CNS1-4 Character Collection)
pub(super) fn lookup_adobe_cns1_to_unicode(cid: u16) -> Option<u32> {
    crate::fonts::cid_mappings::lookup_adobe_cns1(cid)
}

/// Map CID from Adobe-Korea1 character collection to Unicode.
///
/// Adobe-Korea1 contains Korean characters from KS X 1001 and KS X 1002.
/// Reference: Adobe Technical Note #5093 (Adobe-Korea1-2 Character Collection)
pub(super) fn lookup_adobe_korea1_to_unicode(cid: u16) -> Option<u32> {
    crate::fonts::cid_mappings::lookup_adobe_korea1(cid)
}

/// Ascent/descent (as fractions of em) for the 14 standard PDF fonts.
/// Values from Adobe AFM files; used when no FontDescriptor is present.
pub(super) fn standard_font_metrics(base_font: &str) -> Option<(f32, f32)> {
    // Strip subset prefix (e.g. "ABCDEF+Courier" -> "Courier")
    let name = if let Some(pos) = base_font.find('+') {
        &base_font[pos + 1..]
    } else {
        base_font
    };
    match name {
        "Courier" | "Courier-Bold" | "Courier-Oblique" | "Courier-BoldOblique" => {
            Some((0.629, -0.157))
        }
        "Helvetica" | "Helvetica-Bold" | "Helvetica-Oblique" | "Helvetica-BoldOblique" => {
            Some((0.718, -0.207))
        }
        "Times-Roman" => Some((0.683, -0.217)),
        "Times-Bold" => Some((0.676, -0.205)),
        "Times-Italic" => Some((0.683, -0.205)),
        "Times-BoldItalic" => Some((0.683, -0.205)),
        "Symbol" => Some((1.010, -0.293)),
        "ZapfDingbats" => Some((0.820, -0.143)),
        _ => None,
    }
}
