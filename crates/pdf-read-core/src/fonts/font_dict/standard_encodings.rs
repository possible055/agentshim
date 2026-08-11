use super::*;

/// Look up a character in a standard PDF encoding.
///
/// This function provides support for standard PDF encodings including
/// PDFDocEncoding, WinAnsiEncoding, StandardEncoding, and MacRomanEncoding.
///
/// # Arguments
///
/// * `encoding` - The encoding name (e.g., "WinAnsiEncoding", "PDFDocEncoding")
/// * `code` - The character code (0-255)
///
/// # Returns
///
/// Whether an embedded font program's built-in `/Encoding` (`prog_enc`,
/// code→char) looks like a re-indexed subset **cipher** rather than a
/// meaningful text encoding to overlay on the producer-declared named base
/// `std_name`.
///
/// A real encoding (a few non-standard slots over a named base, e.g. space at
/// 0xCA) agrees with the named base on most of the codes they share; a subset
/// cipher — the font's own arbitrary glyph ordering — agrees on almost none,
/// and overlaying it would rewrite every mapped code into mojibake. Decide by
/// agreement: of the codes present in both, fewer than half resolving to the
/// same character means cipher. Empty overlap is treated as NOT a cipher (no
/// evidence either way; keep the prior overlay behaviour).
pub(super) fn builtin_encoding_looks_like_cipher(
    prog_enc: &HashMap<u8, char>,
    std_name: &str,
) -> bool {
    let (mut agree, mut overlap) = (0u32, 0u32);
    for (&code, &ch) in prog_enc {
        if let Some(us) = standard_encoding_lookup(std_name, code) {
            if let Some(sc) = us.chars().next() {
                overlap += 1;
                if sc == ch {
                    agree += 1;
                }
            }
        }
    }
    overlap > 0 && (agree as f32 / overlap as f32) < 0.5
}

/// The Unicode string for this character, or None if not in the encoding.
pub(super) fn standard_encoding_lookup(encoding: &str, code: u8) -> Option<String> {
    match encoding {
        "PDFDocEncoding" => {
            // PDFDocEncoding: superset of ISO Latin-1 with special 128-159 range
            pdfdoc_encoding_lookup(code).map(|c| c.to_string())
        }
        "WinAnsiEncoding" => {
            // ASCII printable range (32-126)
            if (32..=126).contains(&code) {
                return Some((code as char).to_string());
            }

            // WinAnsiEncoding extended range (128-255)
            // Based on Windows-1252 encoding
            let unicode = match code {
                0x80 => '\u{20AC}', // Euro sign
                0x82 => '\u{201A}', // Single low-9 quotation mark
                0x83 => '\u{0192}', // Latin small letter f with hook
                0x84 => '\u{201E}', // Double low-9 quotation mark
                0x85 => '\u{2026}', // Horizontal ellipsis
                0x86 => '\u{2020}', // Dagger
                0x87 => '\u{2021}', // Double dagger
                0x88 => '\u{02C6}', // Modifier letter circumflex accent
                0x89 => '\u{2030}', // Per mille sign
                0x8A => '\u{0160}', // Latin capital letter S with caron
                0x8B => '\u{2039}', // Single left-pointing angle quotation mark
                0x8C => '\u{0152}', // Latin capital ligature OE
                0x8E => '\u{017D}', // Latin capital letter Z with caron
                0x91 => '\u{2018}', // Left single quotation mark
                0x92 => '\u{2019}', // Right single quotation mark
                0x93 => '\u{201C}', // Left double quotation mark
                0x94 => '\u{201D}', // Right double quotation mark
                0x95 => '\u{2022}', // Bullet
                0x96 => '\u{2013}', // En dash
                0x97 => '\u{2014}', // Em dash
                0x98 => '\u{02DC}', // Small tilde
                0x99 => '\u{2122}', // Trade mark sign
                0x9A => '\u{0161}', // Latin small letter s with caron
                0x9B => '\u{203A}', // Single right-pointing angle quotation mark
                0x9C => '\u{0153}', // Latin small ligature oe
                0x9E => '\u{017E}', // Latin small letter z with caron
                0x9F => '\u{0178}', // Latin capital letter Y with diaeresis
                // 0xA0-0xFF: Direct mapping to Unicode (ISO-8859-1)
                _ if code >= 0xA0 => char::from_u32(code as u32)?,
                _ => return None,
            };
            Some(unicode.to_string())
        }
        "StandardEncoding" => {
            // PostScript StandardEncoding per PDF Spec ISO 32000-1:2008, Annex D, Table D.1
            // NOTE: StandardEncoding differs significantly from ISO-8859-1 in the 0xA0-0xFF range.
            // Using ISO-8859-1 fallback here would produce wrong characters for ligatures,
            // smart quotes, accents, and other typographic characters.
            if (32..=126).contains(&code) {
                // Most codes in 32–126 match ASCII, with one notable exception:
                // 0x27 = "quoteright" → U+2019 (RIGHT SINGLE QUOTATION MARK)
                // All other printable ASCII codes are identity-mapped.
                let ch = match code {
                    0x27 => '\u{2019}', // quoteright
                    _ => code as char,
                };
                Some(ch.to_string())
            } else {
                let unicode = match code {
                    // 0xA0-0xAF
                    0xA1 => '\u{00A1}', // exclamdown
                    0xA2 => '\u{00A2}', // cent
                    0xA3 => '\u{00A3}', // sterling
                    0xA4 => '\u{2044}', // fraction (NOT currency ¤)
                    0xA5 => '\u{00A5}', // yen
                    0xA6 => '\u{0192}', // florin (NOT broken bar)
                    0xA7 => '\u{00A7}', // section
                    0xA8 => '\u{00A4}', // currency (NOT dieresis)
                    0xA9 => '\u{0027}', // quotesingle (NOT copyright)
                    0xAA => '\u{201C}', // quotedblleft (NOT ordfeminine)
                    0xAB => '\u{00AB}', // guillemotleft
                    0xAC => '\u{2039}', // guilsinglleft (NOT not-sign)
                    0xAD => '\u{203A}', // guilsinglright (NOT soft-hyphen)
                    0xAE => '\u{FB01}', // fi ligature (NOT registered)
                    0xAF => '\u{FB02}', // fl ligature (NOT macron)
                    // 0xB0-0xBF
                    0xB1 => '\u{2013}', // endash (NOT plus-minus)
                    0xB2 => '\u{2020}', // dagger (NOT superscript 2)
                    0xB3 => '\u{2021}', // daggerdbl (NOT superscript 3)
                    0xB4 => '\u{00B7}', // periodcentered (NOT acute accent)
                    0xB6 => '\u{00B6}', // paragraph
                    0xB7 => '\u{2022}', // bullet (NOT middle dot)
                    0xB8 => '\u{201A}', // quotesinglbase (NOT cedilla)
                    0xB9 => '\u{201E}', // quotedblbase (NOT superscript 1)
                    0xBA => '\u{201D}', // quotedblright (NOT ordmasculine)
                    0xBB => '\u{00BB}', // guillemotright
                    0xBC => '\u{2026}', // ellipsis (NOT one quarter)
                    0xBD => '\u{2030}', // perthousand (NOT one half)
                    0xBF => '\u{00BF}', // questiondown
                    // 0xC0-0xCF — accent marks and modifiers
                    0xC1 => '\u{0060}', // grave (NOT A-grave)
                    0xC2 => '\u{00B4}', // acute (NOT A-circumflex)
                    0xC3 => '\u{02C6}', // circumflex (NOT A-tilde)
                    0xC4 => '\u{02DC}', // tilde (NOT A-dieresis)
                    0xC5 => '\u{00AF}', // macron (NOT A-ring)
                    0xC6 => '\u{02D8}', // breve (NOT AE)
                    0xC7 => '\u{02D9}', // dotaccent (NOT C-cedilla)
                    0xC8 => '\u{00A8}', // dieresis (NOT E-grave)
                    0xCA => '\u{02DA}', // ring (NOT E-circumflex)
                    0xCB => '\u{00B8}', // cedilla (NOT E-dieresis)
                    0xCD => '\u{02DD}', // hungarumlaut (NOT I-acute)
                    0xCE => '\u{02DB}', // ogonek (NOT I-circumflex)
                    0xCF => '\u{02C7}', // caron (NOT I-dieresis)
                    // 0xD0 — em dash
                    0xD0 => '\u{2014}', // emdash (NOT Eth)
                    // 0xE0-0xEF — uppercase special chars
                    0xE1 => '\u{00C6}', // AE (NOT a-acute)
                    0xE3 => '\u{00AA}', // ordfeminine (NOT a-tilde)
                    0xE8 => '\u{0141}', // Lslash (NOT e-grave)
                    0xE9 => '\u{00D8}', // Oslash (NOT e-acute)
                    0xEA => '\u{0152}', // OE (NOT e-circumflex)
                    0xEB => '\u{00BA}', // ordmasculine (NOT e-dieresis)
                    // 0xF0-0xFF — lowercase special chars
                    0xF1 => '\u{00E6}', // ae (NOT n-tilde)
                    0xF5 => '\u{0131}', // dotlessi (NOT o-tilde)
                    0xF8 => '\u{0142}', // lslash (NOT o-stroke)
                    0xF9 => '\u{00F8}', // oslash (NOT u-grave)
                    0xFA => '\u{0153}', // oe (NOT u-acute)
                    0xFB => '\u{00DF}', // germandbls (NOT u-circumflex)
                    _ => return None,
                };
                Some(unicode.to_string())
            }
        }
        "MacRomanEncoding" => {
            // ASCII range is the same
            if (32..=126).contains(&code) {
                Some((code as char).to_string())
            } else {
                // Complete Mac OS Roman encoding per PDF Spec ISO 32000-1:2008, Annex D, Table D.2
                let unicode = match code {
                    // 0x80-0x9F: Accented letters
                    0x80 => '\u{00C4}', // Adieresis
                    0x81 => '\u{00C5}', // Aring
                    0x82 => '\u{00C7}', // Ccedilla
                    0x83 => '\u{00C9}', // Eacute
                    0x84 => '\u{00D1}', // Ntilde
                    0x85 => '\u{00D6}', // Odieresis
                    0x86 => '\u{00DC}', // Udieresis
                    0x87 => '\u{00E1}', // aacute
                    0x88 => '\u{00E0}', // agrave
                    0x89 => '\u{00E2}', // acircumflex
                    0x8A => '\u{00E4}', // adieresis
                    0x8B => '\u{00E3}', // atilde
                    0x8C => '\u{00E5}', // aring
                    0x8D => '\u{00E7}', // ccedilla
                    0x8E => '\u{00E9}', // eacute
                    0x8F => '\u{00E8}', // egrave
                    0x90 => '\u{00EA}', // ecircumflex
                    0x91 => '\u{00EB}', // edieresis
                    0x92 => '\u{00ED}', // iacute
                    0x93 => '\u{00EC}', // igrave
                    0x94 => '\u{00EE}', // icircumflex
                    0x95 => '\u{00EF}', // idieresis
                    0x96 => '\u{00F1}', // ntilde
                    0x97 => '\u{00F3}', // oacute
                    0x98 => '\u{00F2}', // ograve
                    0x99 => '\u{00F4}', // ocircumflex
                    0x9A => '\u{00F6}', // odieresis
                    0x9B => '\u{00F5}', // otilde
                    0x9C => '\u{00FA}', // uacute
                    0x9D => '\u{00F9}', // ugrave
                    0x9E => '\u{00FB}', // ucircumflex
                    0x9F => '\u{00FC}', // udieresis
                    // 0xA0-0xBF: Symbols and punctuation (NOT Latin-1!)
                    0xA0 => '\u{2020}', // dagger (NOT NBSP)
                    0xA1 => '\u{00B0}', // degree (NOT inverted exclamation)
                    0xA2 => '\u{00A2}', // cent
                    0xA3 => '\u{00A3}', // sterling
                    0xA4 => '\u{00A7}', // section (NOT currency sign)
                    0xA5 => '\u{2022}', // bullet (NOT yen)
                    0xA6 => '\u{00B6}', // paragraph (NOT broken bar)
                    0xA7 => '\u{00DF}', // germandbls (NOT section)
                    0xA8 => '\u{00AE}', // registered (NOT dieresis)
                    0xA9 => '\u{00A9}', // copyright
                    0xAA => '\u{2122}', // trademark (NOT ordfeminine)
                    0xAB => '\u{00B4}', // acute (NOT guillemotleft)
                    0xAC => '\u{00A8}', // dieresis (NOT logical not)
                    0xAD => '\u{2260}', // notequal (NOT soft hyphen)
                    0xAE => '\u{00C6}', // AE (NOT registered)
                    0xAF => '\u{00D8}', // Oslash (NOT macron)
                    0xB0 => '\u{221E}', // infinity (NOT degree)
                    0xB1 => '\u{00B1}', // plusminus
                    0xB2 => '\u{2264}', // lessequal (NOT superscript 2)
                    0xB3 => '\u{2265}', // greaterequal (NOT superscript 3)
                    0xB4 => '\u{00A5}', // yen (NOT acute)
                    0xB5 => '\u{00B5}', // mu
                    0xB6 => '\u{2202}', // partialdiff (NOT paragraph)
                    0xB7 => '\u{2211}', // summation (NOT middle dot)
                    0xB8 => '\u{220F}', // product (NOT cedilla)
                    0xB9 => '\u{03C0}', // pi (NOT superscript 1)
                    0xBA => '\u{222B}', // integral (NOT ordmasculine)
                    0xBB => '\u{00AA}', // ordfeminine (NOT guillemotright)
                    0xBC => '\u{00BA}', // ordmasculine (NOT one quarter)
                    0xBD => '\u{2126}', // Omega (NOT one half)
                    0xBE => '\u{00E6}', // ae (NOT three quarters)
                    0xBF => '\u{00F8}', // oslash (NOT inverted question)
                    // 0xC0-0xCF: More symbols and accented capitals
                    0xC0 => '\u{00BF}', // questiondown
                    0xC1 => '\u{00A1}', // exclamdown
                    0xC2 => '\u{00AC}', // logicalnot
                    0xC3 => '\u{221A}', // radical
                    0xC4 => '\u{0192}', // florin
                    0xC5 => '\u{2248}', // approxequal
                    0xC6 => '\u{2206}', // Delta
                    0xC7 => '\u{00AB}', // guillemotleft
                    0xC8 => '\u{00BB}', // guillemotright
                    0xC9 => '\u{2026}', // ellipsis
                    0xCA => '\u{00A0}', // nonbreakingspace
                    0xCB => '\u{00C0}', // Agrave
                    0xCC => '\u{00C3}', // Atilde
                    0xCD => '\u{00D5}', // Otilde
                    0xCE => '\u{0152}', // OE
                    0xCF => '\u{0153}', // oe
                    // 0xD0-0xDF: Dashes, quotes, ligatures
                    0xD0 => '\u{2013}', // endash
                    0xD1 => '\u{2014}', // emdash
                    0xD2 => '\u{201C}', // quotedblleft
                    0xD3 => '\u{201D}', // quotedblright
                    0xD4 => '\u{2018}', // quoteleft
                    0xD5 => '\u{2019}', // quoteright
                    0xD6 => '\u{00F7}', // divide
                    0xD7 => '\u{25CA}', // lozenge
                    0xD8 => '\u{00FF}', // ydieresis
                    0xD9 => '\u{0178}', // Ydieresis
                    0xDA => '\u{2044}', // fraction
                    0xDB => '\u{20AC}', // Euro
                    0xDC => '\u{2039}', // guilsinglleft
                    0xDD => '\u{203A}', // guilsinglright
                    0xDE => '\u{FB01}', // fi ligature
                    0xDF => '\u{FB02}', // fl ligature
                    // 0xE0-0xEF: More symbols and accented capitals
                    0xE0 => '\u{2021}', // daggerdbl
                    0xE1 => '\u{00B7}', // periodcentered
                    0xE2 => '\u{201A}', // quotesinglbase
                    0xE3 => '\u{201E}', // quotedblbase
                    0xE4 => '\u{2030}', // perthousand
                    0xE5 => '\u{00C2}', // Acircumflex
                    0xE6 => '\u{00CA}', // Ecircumflex
                    0xE7 => '\u{00C1}', // Aacute
                    0xE8 => '\u{00CB}', // Edieresis
                    0xE9 => '\u{00C8}', // Egrave
                    0xEA => '\u{00CD}', // Iacute
                    0xEB => '\u{00CE}', // Icircumflex
                    0xEC => '\u{00CF}', // Idieresis
                    0xED => '\u{00CC}', // Igrave
                    0xEE => '\u{00D3}', // Oacute
                    0xEF => '\u{00D4}', // Ocircumflex
                    // 0xF0-0xFF: More accented and special chars
                    0xF0 => '\u{F8FF}', // Apple logo (private use area)
                    0xF1 => '\u{00D2}', // Ograve
                    0xF2 => '\u{00DA}', // Uacute
                    0xF3 => '\u{00DB}', // Ucircumflex
                    0xF4 => '\u{00D9}', // Ugrave
                    0xF5 => '\u{0131}', // dotlessi
                    0xF6 => '\u{02C6}', // circumflex
                    0xF7 => '\u{02DC}', // tilde
                    0xF8 => '\u{00AF}', // macron
                    0xF9 => '\u{02D8}', // breve
                    0xFA => '\u{02D9}', // dotaccent
                    0xFB => '\u{02DA}', // ring
                    0xFC => '\u{00B8}', // cedilla
                    0xFD => '\u{02DD}', // hungarumlaut
                    0xFE => '\u{02DB}', // ogonek
                    0xFF => '\u{02C7}', // caron
                    _ => return None,
                };
                Some(unicode.to_string())
            }
        }
        _ => {
            // Unknown encoding, try identity mapping for ASCII
            if code.is_ascii() && code >= 32 {
                Some((code as char).to_string())
            } else {
                None
            }
        }
    }
}
