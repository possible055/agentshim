use super::*;

/// Map a PDF glyph name to a Unicode character.
///
/// This function implements the Adobe Glyph List (AGL) specification,
/// which defines standard mappings from PostScript glyph names to Unicode.
/// This is essential for parsing /Differences arrays in custom encodings.
///
/// # Arguments
///
/// * `glyph_name` - The PostScript glyph name (e.g., "bullet", "emdash", "Aacute")
///
/// # Returns
///
/// The corresponding Unicode character, or None if the glyph name is not recognized.
///
/// # References
///
/// - Adobe Glyph List Specification: https://github.com/adobe-type-tools/agl-specification
/// - PDF 32000-1:2008, Section 9.6.6.2 (Differences Arrays)
///
/// # Examples
///
/// ```ignore
/// # use pdf_oxide::fonts::font_dict::glyph_name_to_unicode;
/// assert_eq!(glyph_name_to_unicode("bullet"), Some('•'));
/// assert_eq!(glyph_name_to_unicode("emdash"), Some('—'));
/// assert_eq!(glyph_name_to_unicode("A"), Some('A'));
/// assert_eq!(glyph_name_to_unicode("unknown"), None);
/// ```ignore
///
/// Extended glyph names from TeX/math fonts (MSAM, MSBM, Computer Modern, etc.)
/// not present in the standard Adobe Glyph List.
static TEX_MATH_GLYPH_NAMES: phf::Map<&'static str, char> = phf::phf_map! {
    // AMS math symbols (MSAM10, MSBM10)
    "square" => '\u{25A1}',           // WHITE SQUARE
    "squaredot" => '\u{22A1}',        // SQUARED DOT OPERATOR
    "blacksquare" => '\u{25A0}',      // BLACK SQUARE
    "dblarrowup" => '\u{21C8}',       // UPWARDS PAIRED ARROWS
    "dblarrowdwn" => '\u{21CA}',      // DOWNWARDS PAIRED ARROWS
    "dblarrowleft" => '\u{21C7}',     // LEFTWARDS PAIRED ARROWS
    "dblarrowright" => '\u{21C9}',    // RIGHTWARDS PAIRED ARROWS
    "triangle" => '\u{25B3}',         // WHITE UP-POINTING TRIANGLE
    "triangledown" => '\u{25BD}',     // WHITE DOWN-POINTING TRIANGLE
    "triangleleft" => '\u{25C1}',     // WHITE LEFT-POINTING TRIANGLE
    "triangleright" => '\u{25B7}',    // WHITE RIGHT-POINTING TRIANGLE
    "blacktriangle" => '\u{25B2}',    // BLACK UP-POINTING TRIANGLE
    "blacktriangledown" => '\u{25BC}',// BLACK DOWN-POINTING TRIANGLE
    "blacktriangleleft" => '\u{25C0}',// BLACK LEFT-POINTING TRIANGLE
    "blacktriangleright" => '\u{25B6}',// BLACK RIGHT-POINTING TRIANGLE
    "diamond" => '\u{25C7}',          // WHITE DIAMOND
    "blackdiamond" => '\u{25C6}',     // BLACK DIAMOND
    "circle" => '\u{25CB}',           // WHITE CIRCLE
    "bullet1" => '\u{2219}',          // BULLET OPERATOR
    "star" => '\u{22C6}',             // STAR OPERATOR
    "bigstar" => '\u{2605}',          // BLACK STAR
    "checkmark" => '\u{2713}',        // CHECK MARK
    "maltese" => '\u{2720}',          // MALTESE CROSS
    // TeX arrows
    "arrowleft" => '\u{2190}',        // LEFTWARDS ARROW
    "arrowright" => '\u{2192}',       // RIGHTWARDS ARROW
    "arrowup" => '\u{2191}',          // UPWARDS ARROW
    "arrowdown" => '\u{2193}',        // DOWNWARDS ARROW
    "arrowboth" => '\u{2194}',        // LEFT RIGHT ARROW
    "arrowdblup" => '\u{21D1}',       // UPWARDS DOUBLE ARROW
    "arrowdbldown" => '\u{21D3}',     // DOWNWARDS DOUBLE ARROW
    "arrowdblleft" => '\u{21D0}',     // LEFTWARDS DOUBLE ARROW
    "arrowdblright" => '\u{21D2}',    // RIGHTWARDS DOUBLE ARROW
    "arrowdblboth" => '\u{21D4}',     // LEFT RIGHT DOUBLE ARROW
    // TeX math operators
    "langle" => '\u{27E8}',           // MATHEMATICAL LEFT ANGLE BRACKET
    "rangle" => '\u{27E9}',           // MATHEMATICAL RIGHT ANGLE BRACKET
    "lfloor" => '\u{230A}',           // LEFT FLOOR
    "rfloor" => '\u{230B}',           // RIGHT FLOOR
    "lceil" => '\u{2308}',            // LEFT CEILING
    "rceil" => '\u{2309}',            // RIGHT CEILING
    "emptyset" => '\u{2205}',         // EMPTY SET
    "infty" => '\u{221E}',            // INFINITY (alias)
    "nabla" => '\u{2207}',            // NABLA
    "partial" => '\u{2202}',          // PARTIAL DIFFERENTIAL
    "forall" => '\u{2200}',           // FOR ALL
    "exists" => '\u{2203}',           // THERE EXISTS
    "neg" => '\u{00AC}',              // NOT SIGN
    "backslash" => '\u{005C}',        // REVERSE SOLIDUS
    "prime" => '\u{2032}',            // PRIME
    "natural" => '\u{266E}',          // MUSIC NATURAL SIGN
    "flat" => '\u{266D}',             // MUSIC FLAT SIGN
    "sharp" => '\u{266F}',            // MUSIC SHARP SIGN
};

/// Convert a Shift-JIS encoded byte sequence (1 or 2 bytes) to a Unicode character.
/// Uses the encoding_rs crate for correct, complete Shift-JIS decoding.
pub(super) fn shift_jis_to_unicode(code: u16) -> Option<char> {
    let bytes = if code <= 0xFF {
        vec![code as u8]
    } else {
        vec![(code >> 8) as u8, (code & 0xFF) as u8]
    };
    let (decoded, _, had_errors) = encoding_rs::SHIFT_JIS.decode(&bytes);
    if had_errors {
        return None;
    }
    let mut chars = decoded.chars();
    let c = chars.next()?;
    // Ensure only one character was produced
    if chars.next().is_some() {
        return None;
    }
    Some(c)
}

/// Normalize CJK radical "presentation" codepoints to their canonical unified
/// ideograph: CJK Radicals Supplement (U+2E80–2EFF) and Kangxi Radicals
/// (U+2F00–2FDF). These blocks hold the radical glyphs used in dictionaries and
/// are never part of running text — but a font cmap that maps a glyph shared
/// between a radical and its ideograph to the *radical* codepoint (and a
/// GID→Unicode reverse lookup that then prefers it) surfaces e.g. 欠→⽋, 立→⽴.
/// NFKC carries each radical to its ideograph; only chars inside the two radical
/// blocks are touched, so legitimate text (incl. fullwidth forms) is unchanged.
/// Fast-path returns the input untouched when it contains no radical-block char.
pub(super) fn normalize_cjk_radical_forms(s: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    fn is_radical(c: char) -> bool {
        // CJK Radicals Supplement (U+2E80–2EFF) + Kangxi Radicals (U+2F00–2FDF),
        // which are contiguous, so a single range covers both blocks.
        matches!(c as u32, 0x2E80..=0x2FDF)
    }
    if !s.chars().any(is_radical) {
        return s.to_string();
    }
    s.chars()
        .flat_map(|c| {
            if is_radical(c) {
                // NFKC-decompose just this radical glyph to its ideograph.
                Box::new(c.nfkc()) as Box<dyn Iterator<Item = char>>
            } else {
                Box::new(std::iter::once(c)) as Box<dyn Iterator<Item = char>>
            }
        })
        .collect()
}

pub(crate) fn glyph_name_to_unicode(glyph_name: &str) -> Option<char> {
    // Priority 1: Adobe Glyph List (AGL) lookup - O(1) with perfect hash
    // PDF Spec: ISO 32000-1:2008, Section 9.10.2
    if let Some(&unicode_char) = crate::fonts::adobe_glyph_list::ADOBE_GLYPH_LIST.get(glyph_name) {
        return Some(unicode_char);
    }

    // Priority 1b: Extended glyph names from TeX/math fonts (MSAM, MSBM, etc.)
    // These are well-known glyph names not in the standard AGL but common in
    // academic/mathematical PDFs generated by TeX/LaTeX.
    if let Some(&unicode_char) = TEX_MATH_GLYPH_NAMES.get(glyph_name) {
        return Some(unicode_char);
    }

    // Priority 2: Underscore-delimited compound glyph names (AGL spec section 2)
    // e.g. "f_f" → 'f'+'f', "f_i" → 'f'+'i', "T_h" → 'T'+'h'
    // Return the first component character for single-char return type
    if glyph_name.contains('_') {
        let parts: Vec<&str> = glyph_name.split('_').collect();
        if let Some(first) = parts.first() {
            if let Some(&ch) = crate::fonts::adobe_glyph_list::ADOBE_GLYPH_LIST.get(*first) {
                return Some(ch);
            }
        }
    }

    // Priority 3 (#535 follow-up): delegate to the unified fallback chain
    // in `character_mapper::glyph_name_to_unicode`. The newer chain adds:
    //   - Variant-suffix stripping (`A.sc`, `bullet.alt`, `fi.001`) — common in
    //     subset fonts where producers append stylistic-variant tags.
    //   - Stricter `uniXXXX` (exactly 4 hex, no control chars) and `uXXXXX`
    //     (4..6 hex, no surrogates, no control chars) validation.
    // This brings simple-font / Type1 / CFF / Differences-array callers (which
    // route through this `font_dict::glyph_name_to_unicode` entry) onto the
    // same fallback chain as the #535 Type0 Identity-H path. Inline-
    // image font streams (PDF spec §8.9.7) that resolve glyph names by this
    // path inherit the same behaviour transparently — no separate inline-image
    // codepath exists in this crate; inline images per spec carry only image
    // data, but any future inline-image font-resolution callsite will use this
    // unified chain by construction.
    if let Some(unicode_str) = crate::fonts::character_mapper::glyph_name_to_unicode(glyph_name) {
        // The newer chain returns `String` (to allow multi-codepoint AGL
        // entries like ligatures, though current AGL values are all single
        // BMP codepoints). For the legacy `Option<char>` surface we only
        // forward if the result is exactly one `char` — multi-codepoint
        // results are handled by `glyph_name_to_unicode_string` below.
        let mut chars = unicode_str.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            return Some(c);
        }
    }

    // Unknown glyph name - not in AGL and not a recognized format
    log::debug!(
        "Unknown glyph name not in Adobe Glyph List: '{}'",
        glyph_name
    );
    None
}

/// Resolve a glyph name to a Unicode string, handling compound names.
///
/// Like `glyph_name_to_unicode` but returns a full String for compound glyph names
/// (underscore-delimited per AGL spec, e.g. "f_f" → "ff", "f_f_i" → "ffi").
pub(crate) fn glyph_name_to_unicode_string(glyph_name: &str) -> Option<String> {
    // Try single char lookup first
    if let Some(ch) = glyph_name_to_unicode(glyph_name) {
        return Some(ch.to_string());
    }

    // Handle underscore-delimited compound names (AGL spec section 2)
    if glyph_name.contains('_') {
        let mut result = String::new();
        for part in glyph_name.split('_') {
            // If any component is unknown, fail entirely.
            let ch = glyph_name_to_unicode(part)?;
            result.push(ch);
        }
        if !result.is_empty() {
            return Some(result);
        }
    }

    // Final fallback (#535 follow-up): unified chain — variant-suffix
    // stripping + strict uniXXXX / uXXXXX synth. Returns the full `String` shape
    // (multi-codepoint AGL entries are forwarded unchanged).
    crate::fonts::character_mapper::glyph_name_to_unicode(glyph_name)
}

/// AGL Unicode for the closed set of punctuation glyph names that the Item 1
/// fix recovers (ISO 32000-1 §9.10.2(a)+(b)). Restricted deliberately to these
/// four names — generalising to all AGL names would re-introduce regression risk
/// against fonts whose ToUnicode is genuinely authoritative.
///
/// `period`→`"."`, `comma`→`","`, `hyphen`→`"-"`, `minus`→`"\u{2212}"`;
/// anything else → `None`.
pub(super) fn punctuation_unicode_for_glyph_name(name: &str) -> Option<&'static str> {
    match name {
        "period" => Some("."),
        "comma" => Some(","),
        "hyphen" => Some("-"),
        "minus" => Some("\u{2212}"),
        _ => None,
    }
}

/// True iff `s` is a single character that is a "non-sensible symbol" — i.e. a
/// symbol/arrow/math glyph that is clearly not the punctuation a `period`/
/// `comma`/`hyphen`/`minus` glyph name denotes. This gates the Item 1
/// interceptions so they fire only when an upstream decode produced a wrong
/// symbol (e.g. U+00AC `¬` or an arrow/math char) for a punctuation-named code.
///
/// Covers the Latin-1 supplement symbol range (U+00A1..=U+00BF, which includes
/// U+00AC `¬`) and the arrow/math/symbol blocks (U+2190..=U+2BFF). Returns
/// `false` for `.`, `,`, `-`, ASCII digits, and any alphabetic letter.
pub(super) fn is_non_sensible_symbol(s: &str) -> bool {
    let mut chars = s.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        // Empty or multi-char strings are not single non-sensible symbols.
        return false;
    };
    // Sensible by construction: letters and ASCII digits/punctuation.
    if c.is_alphabetic() || c.is_ascii_digit() || c.is_ascii_punctuation() {
        return false;
    }
    let cp = c as u32;
    // Latin-1 supplement symbol range (¬ = U+00AC, etc.) and the
    // arrow / mathematical-operator / misc-symbol blocks.
    matches!(cp, 0x00A1..=0x00BF | 0x2190..=0x2BFF)
}

// Removed old implementation - replaced with compact AGL lookup above
// Old code: ~350 lines of match arms with ~200 hardcoded glyphs
// New code: 4281 glyphs from official Adobe Glyph List via perfect hash map
#[allow(dead_code)]
pub(super) fn _old_glyph_name_to_unicode_removed() {
    // This function body intentionally left empty.
    // The old match-based implementation has been replaced with
    // a lookup in the complete Adobe Glyph List static map.
    // See super::adobe_glyph_list::ADOBE_GLYPH_LIST for the new implementation.
}

// Old implementation removed - was 350+ lines of hardcoded match arms
// Now using complete Adobe Glyph List with 4281 entries from adobe_glyph_list module

/// Check if a character is a ligature.
///
/// This function identifies Unicode ligature characters (U+FB00 to U+FB06)
/// that are commonly used in PDFs for typographic ligatures.
///
/// # Arguments
///
/// * `c` - The character to check
///
/// # Returns
///
/// `true` if the character is a ligature, `false` otherwise.
///
/// # Examples
///
/// ```ignore
/// # use pdf_oxide::fonts::font_dict::is_ligature_char;
/// assert_eq!(is_ligature_char('ﬁ'), true); // U+FB01
/// assert_eq!(is_ligature_char('ﬂ'), true); // U+FB02
/// assert_eq!(is_ligature_char('A'), false);
/// ```ignore
pub(super) fn is_ligature_char(c: char) -> bool {
    matches!(
        c,
        'ﬀ' |  // ff  - U+FB00
        'ﬁ' |  // fi  - U+FB01
        'ﬂ' |  // fl  - U+FB02
        'ﬃ' |  // ffi - U+FB03
        'ﬄ' |  // ffl - U+FB04
        'ﬅ' |  // st (long s + t) - U+FB05
        'ﬆ' // st - U+FB06
    )
}

/// Expand a ligature character to its ASCII equivalent.
///
/// This function handles the Unicode ligature characters (U+FB00 to U+FB06)
/// and expands them to their multi-character ASCII equivalents.
///
/// # Arguments
///
/// * `c` - The character to potentially expand
///
/// # Returns
///
/// The expanded string if `c` is a ligature, None otherwise.
///
/// # Examples
///
/// ```ignore
/// # use pdf_oxide::fonts::font_dict::expand_ligature_char;
/// assert_eq!(expand_ligature_char('ﬁ'), Some("fi"));
/// assert_eq!(expand_ligature_char('ﬂ'), Some("fl"));
/// assert_eq!(expand_ligature_char('A'), None);
/// ```ignore
pub(super) fn expand_ligature_char(c: char) -> Option<&'static str> {
    match c {
        'ﬀ' => Some("ff"),  // U+FB00
        'ﬁ' => Some("fi"),  // U+FB01
        'ﬂ' => Some("fl"),  // U+FB02
        'ﬃ' => Some("ffi"), // U+FB03
        'ﬄ' => Some("ffl"), // U+FB04
        'ﬅ' => Some("st"),  // U+FB05 (long s + t)
        'ﬆ' => Some("st"),  // U+FB06
        _ => None,
    }
}
