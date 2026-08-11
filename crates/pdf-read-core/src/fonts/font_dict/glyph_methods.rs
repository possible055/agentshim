use super::*;

impl FontInfo {
    /// Map a Glyph ID (GID) to a standard PostScript glyph name.
    ///
    /// This is used as a fallback for Type0 fonts without ToUnicode CMaps.
    /// For ASCII range GIDs (32-126), maps to standard PostScript glyph names
    /// that can be looked up in the Adobe Glyph List.
    ///
    /// Phase 1.2: Adobe Glyph List Fallback
    ///
    /// # Arguments
    ///
    /// * `gid` - The Glyph ID to map (typically 0x20-0x7E for ASCII)
    ///
    /// # Returns
    ///
    /// The standard glyph name if GID is in the ASCII range, None otherwise
    ///
    /// # Examples
    ///
    /// ```ignore
    /// assert_eq!(FontInfo::gid_to_standard_glyph_name(0x41), Some("A"));
    /// assert_eq!(FontInfo::gid_to_standard_glyph_name(0x20), Some("space"));
    /// assert_eq!(FontInfo::gid_to_standard_glyph_name(0xFFFF), None);
    /// ```
    pub fn gid_to_standard_glyph_name(gid: u16) -> Option<&'static str> {
        // Map GIDs to standard PostScript glyph names across multiple ranges:
        // - ASCII printable range (0x20-0x7E)
        // - Extended Latin / Windows-1252 range (0x80-0xFF)
        // - Latin-1 Supplement range (0xA0-0xFF)
        match gid {
            // Control characters and whitespace (32-33)
            0x20 => Some("space"),
            0x21 => Some("exclam"),
            0x22 => Some("quotedbl"),
            0x23 => Some("numbersign"),
            0x24 => Some("dollar"),
            0x25 => Some("percent"),
            0x26 => Some("ampersand"),
            0x27 => Some("quoteright"),
            0x28 => Some("parenleft"),
            0x29 => Some("parenright"),
            0x2A => Some("asterisk"),
            0x2B => Some("plus"),
            0x2C => Some("comma"),
            0x2D => Some("hyphen"),
            0x2E => Some("period"),
            0x2F => Some("slash"),
            // Digits (48-57)
            0x30 => Some("zero"),
            0x31 => Some("one"),
            0x32 => Some("two"),
            0x33 => Some("three"),
            0x34 => Some("four"),
            0x35 => Some("five"),
            0x36 => Some("six"),
            0x37 => Some("seven"),
            0x38 => Some("eight"),
            0x39 => Some("nine"),
            // Punctuation (58-64)
            0x3A => Some("colon"),
            0x3B => Some("semicolon"),
            0x3C => Some("less"),
            0x3D => Some("equal"),
            0x3E => Some("greater"),
            0x3F => Some("question"),
            0x40 => Some("at"),
            // Uppercase letters (65-90)
            0x41 => Some("A"),
            0x42 => Some("B"),
            0x43 => Some("C"),
            0x44 => Some("D"),
            0x45 => Some("E"),
            0x46 => Some("F"),
            0x47 => Some("G"),
            0x48 => Some("H"),
            0x49 => Some("I"),
            0x4A => Some("J"),
            0x4B => Some("K"),
            0x4C => Some("L"),
            0x4D => Some("M"),
            0x4E => Some("N"),
            0x4F => Some("O"),
            0x50 => Some("P"),
            0x51 => Some("Q"),
            0x52 => Some("R"),
            0x53 => Some("S"),
            0x54 => Some("T"),
            0x55 => Some("U"),
            0x56 => Some("V"),
            0x57 => Some("W"),
            0x58 => Some("X"),
            0x59 => Some("Y"),
            0x5A => Some("Z"),
            // Brackets (91-96)
            0x5B => Some("bracketleft"),
            0x5C => Some("backslash"),
            0x5D => Some("bracketright"),
            0x5E => Some("asciicircum"),
            0x5F => Some("underscore"),
            0x60 => Some("quoteleft"),
            // Lowercase letters (97-122)
            0x61 => Some("a"),
            0x62 => Some("b"),
            0x63 => Some("c"),
            0x64 => Some("d"),
            0x65 => Some("e"),
            0x66 => Some("f"),
            0x67 => Some("g"),
            0x68 => Some("h"),
            0x69 => Some("i"),
            0x6A => Some("j"),
            0x6B => Some("k"),
            0x6C => Some("l"),
            0x6D => Some("m"),
            0x6E => Some("n"),
            0x6F => Some("o"),
            0x70 => Some("p"),
            0x71 => Some("q"),
            0x72 => Some("r"),
            0x73 => Some("s"),
            0x74 => Some("t"),
            0x75 => Some("u"),
            0x76 => Some("v"),
            0x77 => Some("w"),
            0x78 => Some("x"),
            0x79 => Some("y"),
            0x7A => Some("z"),
            // Braces (123-126)
            0x7B => Some("braceleft"),
            0x7C => Some("bar"),
            0x7D => Some("braceright"),
            0x7E => Some("asciitilde"),

            // ==================================================================================
            // Extended Latin / Windows-1252 range (0x80-0xFF)
            // ==================================================================================
            // These mappings cover the extended ASCII characters commonly found in Western
            // European PDFs. When a Type0 font with Identity CMap encounters these GIDs,
            // we map them to their standard PostScript glyph names for AGL lookup.
            //
            // Per PDF Spec ISO 32000-1:2008 Section 9.10.2, when ToUnicode CMap is unavailable,
            // readers may use glyph name lookup as a fallback mechanism.

            // 0x80-0x8F: Windows-1252 extended control characters and symbols
            0x80 => Some("euro"), // U+20AC (Euro sign)
            // 0x81: undefined in Windows-1252
            0x82 => Some("quotesinglbase"), // U+201A (Single low quotation mark)
            0x83 => Some("florin"),         // U+0192 (Latin small letter f with hook)
            0x84 => Some("quotedblbase"),   // U+201E (Double low quotation mark)
            0x85 => Some("ellipsis"),       // U+2026 (Horizontal ellipsis)
            0x86 => Some("dagger"),         // U+2020 (Dagger)
            0x87 => Some("daggerdbl"),      // U+2021 (Double dagger)
            0x88 => Some("circumflex"),     // U+02C6 (Modifier letter circumflex accent)
            0x89 => Some("perthousand"),    // U+2030 (Per mille sign)
            0x8A => Some("Scaron"),         // U+0160 (Latin capital letter S with caron)
            0x8B => Some("guilsinglleft"),  // U+2039 (Single left-pointing angle quotation mark)
            0x8C => Some("OE"),             // U+0152 (Latin capital ligature OE)
            // 0x8D: undefined in Windows-1252
            0x8E => Some("Zcaron"), // U+017D (Latin capital letter Z with caron)
            // 0x8F: undefined in Windows-1252

            // 0x90-0x9F: Windows-1252 smart quotes, dashes, and accents
            // 0x90: undefined in Windows-1252
            0x91 => Some("quoteleft"), // U+2018 (Left single quotation mark)
            0x92 => Some("quoteright"), // U+2019 (Right single quotation mark)
            0x93 => Some("quotedblleft"), // U+201C (Left double quotation mark)
            0x94 => Some("quotedblright"), // U+201D (Right double quotation mark)
            0x95 => Some("bullet"),    // U+2022 (Bullet)
            0x96 => Some("endash"),    // U+2013 (En dash)
            0x97 => Some("emdash"),    // U+2014 (Em dash)
            0x98 => Some("tilde"),     // U+02DC (Small tilde)
            0x99 => Some("trademark"), // U+2122 (Trade mark sign)
            0x9A => Some("scaron"),    // U+0161 (Latin small letter s with caron)
            0x9B => Some("guilsinglright"), // U+203A (Single right-pointing angle quotation mark)
            0x9C => Some("oe"),        // U+0153 (Latin small ligature oe)
            // 0x9D: undefined in Windows-1252
            0x9E => Some("zcaron"), // U+017E (Latin small letter z with caron)
            0x9F => Some("Ydieresis"), // U+0178 (Latin capital letter Y with diaeresis)

            // 0xA0-0xFF: Latin-1 Supplement (ISO 8859-1)
            // Most of these are direct character mappings (À-ÿ)
            // Implement programmatically for common characters and fallback to glyph name generation
            0xA0 => Some("space"),          // U+00A0 (No-break space)
            0xA1 => Some("exclamdown"),     // U+00A1 (Inverted exclamation mark)
            0xA2 => Some("cent"),           // U+00A2 (Cent sign)
            0xA3 => Some("sterling"),       // U+00A3 (Pound sign)
            0xA4 => Some("currency"),       // U+00A4 (Currency sign)
            0xA5 => Some("yen"),            // U+00A5 (Yen sign)
            0xA6 => Some("brokenbar"),      // U+00A6 (Broken bar)
            0xA7 => Some("section"),        // U+00A7 (Section sign)
            0xA8 => Some("dieresis"),       // U+00A8 (Diaeresis)
            0xA9 => Some("copyright"),      // U+00A9 (Copyright sign)
            0xAA => Some("ordfeminine"),    // U+00AA (Feminine ordinal indicator)
            0xAB => Some("guillemotleft"),  // U+00AB (Left-pointing double angle quotation mark)
            0xAC => Some("logicalnot"),     // U+00AC (Not sign)
            0xAD => Some("uni00AD"),        // U+00AD (Soft hyphen)
            0xAE => Some("registered"),     // U+00AE (Registered sign)
            0xAF => Some("macron"),         // U+00AF (Macron)
            0xB0 => Some("degree"),         // U+00B0 (Degree sign)
            0xB1 => Some("plusminus"),      // U+00B1 (Plus-minus sign)
            0xB2 => Some("twosuperior"),    // U+00B2 (Superscript two)
            0xB3 => Some("threesuperior"),  // U+00B3 (Superscript three)
            0xB4 => Some("acute"),          // U+00B4 (Acute accent)
            0xB5 => Some("mu"),             // U+00B5 (Micro sign)
            0xB6 => Some("paragraph"),      // U+00B6 (Pilcrow)
            0xB7 => Some("middot"),         // U+00B7 (Middle dot)
            0xB8 => Some("cedilla"),        // U+00B8 (Cedilla)
            0xB9 => Some("onesuperior"),    // U+00B9 (Superscript one)
            0xBA => Some("ordmasculine"),   // U+00BA (Masculine ordinal indicator)
            0xBB => Some("guillemotright"), // U+00BB (Right-pointing double angle quotation mark)
            0xBC => Some("onequarter"),     // U+00BC (Vulgar fraction one quarter)
            0xBD => Some("onehalf"),        // U+00BD (Vulgar fraction one half)
            0xBE => Some("threequarters"),  // U+00BE (Vulgar fraction three quarters)
            0xBF => Some("questiondown"),   // U+00BF (Inverted question mark)

            // 0xC0-0xFE: Latin-1 Supplement letters (À-þ)
            // These map directly to their Unicode equivalents via standard PostScript names
            // Format: glyph name is the Unicode character itself (e.g., "Agrave" for U+00C0)
            0xC0 => Some("Agrave"), // U+00C0 (Latin capital letter A with grave)
            0xC1 => Some("Aacute"), // U+00C1 (Latin capital letter A with acute)
            0xC2 => Some("Acircumflex"), // U+00C2 (Latin capital letter A with circumflex)
            0xC3 => Some("Atilde"), // U+00C3 (Latin capital letter A with tilde)
            0xC4 => Some("Adieresis"), // U+00C4 (Latin capital letter A with diaeresis)
            0xC5 => Some("Aring"),  // U+00C5 (Latin capital letter A with ring above)
            0xC6 => Some("AE"),     // U+00C6 (Latin capital letter AE)
            0xC7 => Some("Ccedilla"), // U+00C7 (Latin capital letter C with cedilla)
            0xC8 => Some("Egrave"), // U+00C8 (Latin capital letter E with grave)
            0xC9 => Some("Eacute"), // U+00C9 (Latin capital letter E with acute)
            0xCA => Some("Ecircumflex"), // U+00CA (Latin capital letter E with circumflex)
            0xCB => Some("Edieresis"), // U+00CB (Latin capital letter E with diaeresis)
            0xCC => Some("Igrave"), // U+00CC (Latin capital letter I with grave)
            0xCD => Some("Iacute"), // U+00CD (Latin capital letter I with acute)
            0xCE => Some("Icircumflex"), // U+00CE (Latin capital letter I with circumflex)
            0xCF => Some("Idieresis"), // U+00CF (Latin capital letter I with diaeresis)
            0xD0 => Some("Eth"),    // U+00D0 (Latin capital letter Eth)
            0xD1 => Some("Ntilde"), // U+00D1 (Latin capital letter N with tilde)
            0xD2 => Some("Ograve"), // U+00D2 (Latin capital letter O with grave)
            0xD3 => Some("Oacute"), // U+00D3 (Latin capital letter O with acute)
            0xD4 => Some("Ocircumflex"), // U+00D4 (Latin capital letter O with circumflex)
            0xD5 => Some("Otilde"), // U+00D5 (Latin capital letter O with tilde)
            0xD6 => Some("Odieresis"), // U+00D6 (Latin capital letter O with diaeresis)
            0xD7 => Some("multiply"), // U+00D7 (Multiplication sign)
            0xD8 => Some("Oslash"), // U+00D8 (Latin capital letter O with stroke)
            0xD9 => Some("Ugrave"), // U+00D9 (Latin capital letter U with grave)
            0xDA => Some("Uacute"), // U+00DA (Latin capital letter U with acute)
            0xDB => Some("Ucircumflex"), // U+00DB (Latin capital letter U with circumflex)
            0xDC => Some("Udieresis"), // U+00DC (Latin capital letter U with diaeresis)
            0xDD => Some("Yacute"), // U+00DD (Latin capital letter Y with acute)
            0xDE => Some("Thorn"),  // U+00DE (Latin capital letter Thorn)
            0xDF => Some("germandbls"), // U+00DF (Latin small letter sharp s)
            0xE0 => Some("agrave"), // U+00E0 (Latin small letter a with grave)
            0xE1 => Some("aacute"), // U+00E1 (Latin small letter a with acute)
            0xE2 => Some("acircumflex"), // U+00E2 (Latin small letter a with circumflex)
            0xE3 => Some("atilde"), // U+00E3 (Latin small letter a with tilde)
            0xE4 => Some("adieresis"), // U+00E4 (Latin small letter a with diaeresis)
            0xE5 => Some("aring"),  // U+00E5 (Latin small letter a with ring above)
            0xE6 => Some("ae"),     // U+00E6 (Latin small letter ae)
            0xE7 => Some("ccedilla"), // U+00E7 (Latin small letter c with cedilla)
            0xE8 => Some("egrave"), // U+00E8 (Latin small letter e with grave)
            0xE9 => Some("eacute"), // U+00E9 (Latin small letter e with acute)
            0xEA => Some("ecircumflex"), // U+00EA (Latin small letter e with circumflex)
            0xEB => Some("edieresis"), // U+00EB (Latin small letter e with diaeresis)
            0xEC => Some("igrave"), // U+00EC (Latin small letter i with grave)
            0xED => Some("iacute"), // U+00ED (Latin small letter i with acute)
            0xEE => Some("icircumflex"), // U+00EE (Latin small letter i with circumflex)
            0xEF => Some("idieresis"), // U+00EF (Latin small letter i with diaeresis)
            0xF0 => Some("eth"),    // U+00F0 (Latin small letter eth)
            0xF1 => Some("ntilde"), // U+00F1 (Latin small letter n with tilde)
            0xF2 => Some("ograve"), // U+00F2 (Latin small letter o with grave)
            0xF3 => Some("oacute"), // U+00F3 (Latin small letter o with acute)
            0xF4 => Some("ocircumflex"), // U+00F4 (Latin small letter o with circumflex)
            0xF5 => Some("otilde"), // U+00F5 (Latin small letter o with tilde)
            0xF6 => Some("odieresis"), // U+00F6 (Latin small letter o with diaeresis)
            0xF7 => Some("divide"), // U+00F7 (Division sign)
            0xF8 => Some("oslash"), // U+00F8 (Latin small letter o with stroke)
            0xF9 => Some("ugrave"), // U+00F9 (Latin small letter u with grave)
            0xFA => Some("uacute"), // U+00FA (Latin small letter u with acute)
            0xFB => Some("ucircumflex"), // U+00FB (Latin small letter u with circumflex)
            0xFC => Some("udieresis"), // U+00FC (Latin small letter u with diaeresis)
            0xFD => Some("yacute"), // U+00FD (Latin small letter y with acute)
            0xFE => Some("thorn"),  // U+00FE (Latin small letter thorn)
            0xFF => Some("ydieresis"), // U+00FF (Latin small letter y with diaeresis)

            // All other GIDs not in the supported ranges
            _ => None,
        }
    }

    /// Get the pre-computed byte→char lookup table for OneByte (simple) fonts.
    /// Built lazily on first call by running `char_to_unicode` for all 256 byte values.
    /// Returns a 256-element array: non-'\0' = single printable char, '\0' = needs fallback.
    /// Control chars (except tab/newline/cr), multi-char, and \u{FFFD} are stored as '\0'.
    pub fn get_byte_to_char_table(&self) -> &[char; 256] {
        self.byte_to_char_table.get_or_init(|| {
            let mut tbl = ['\0'; 256];
            for i in 0..=255u8 {
                if let Some(s) = self.char_to_unicode(i as u32) {
                    let mut chars = s.chars();
                    if let Some(c) = chars.next() {
                        if chars.next().is_none()
                            && c != '\u{FFFD}'
                            && (c >= '\x20' || c == '\t' || c == '\n' || c == '\r')
                        {
                            tbl[i as usize] = c;
                        }
                        // Multi-char, replacement, or control char: leave as '\0'
                    }
                }
            }
            tbl
        })
    }

    /// Pre-computed byte→width lookup for simple (non-Type0) fonts.
    /// Returns a 256-entry array where index i = glyph width for byte i.
    /// Eliminates per-byte bounds check and subtraction in advance_position.
    #[inline]
    pub fn get_byte_to_width_table(&self) -> &[f32; 256] {
        self.byte_to_width_table.get_or_init(|| {
            let mut tbl = [self.default_width; 256];
            if let Some(widths) = &self.widths {
                if let Some(first_char) = self.first_char {
                    for (idx, &w) in widths.iter().enumerate() {
                        let code = first_char as usize + idx;
                        if code < 256 {
                            tbl[code] = w;
                        }
                    }
                }
            } else {
                // Standard-14 fonts ship without /Widths arrays; per PDF
                // spec (ISO 32000-1 §9.6.2.2) readers must use built-in
                // metrics. get_standard_font_width returns Some(w) for
                // Helvetica/Times/Courier variants and None otherwise,
                // so non-standard fonts retain the default_width fallback.
                for byte_code in 0..256u16 {
                    if let Some(w) = self.get_standard_font_width(byte_code) {
                        tbl[byte_code as usize] = w;
                    }
                }
            }
            tbl
        })
    }
}
