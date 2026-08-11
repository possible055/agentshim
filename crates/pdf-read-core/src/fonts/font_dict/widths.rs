use super::*;

impl FontInfo {
    /// Map a character code to a Unicode string.
    ///
    /// Priority:
    /// 1. ToUnicode CMap (most accurate)
    /// 2. Built-in encoding
    /// 3. Symbol font encoding (for Symbol/ZapfDingbats fonts)
    /// 4. Ligature expansion (for ligature characters)
    /// 5. Identity mapping (as fallback)
    ///
    /// # Arguments
    ///
    /// * `char_code` - The character code from the PDF content stream
    ///
    /// # Returns
    ///
    /// The Unicode string for this character, or None if no mapping exists.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use pdf_oxide::fonts::FontInfo;
    /// # fn example(font: &FontInfo) {
    /// if let Some(unicode) = font.char_to_unicode(0x41) {
    ///     println!("Character: {}", unicode); // Should print "A"
    /// }
    /// # }
    /// ```
    /// Convert a character code to Unicode string.
    ///
    /// Per PDF Spec ISO 32000-1:2008, Section 9.10.2 "Mapping Character Codes to Unicode Values":
    ///
    /// Priority order (STRICTLY FOLLOWED):
    /// 1. ToUnicode CMap (if present) - highest priority, NO EXCEPTIONS
    /// 2. Predefined encodings for simple fonts with standard glyphs
    /// 3. Font descriptor's symbolic flag + built-in encoding (e.g., Symbol, ZapfDingbats)
    /// 4. Font's /Encoding + /Differences
    ///
    /// IMPORTANT: We do NOT apply heuristics to override ToUnicode. If the PDF has
    /// a buggy ToUnicode CMap, that is a PDF authoring error, not our responsibility
    /// to "fix" by guessing what the author meant.
    /// Get glyph width for a character code.
    ///
    /// Returns width in 1000ths of em (PDF units) per PDF Spec ISO 32000-1:2008, Section 9.7.4.
    /// Must be multiplied by (font_size / 1000) to get actual width in user space units.
    ///
    /// # Arguments
    ///
    /// * `char_code` - Character code from PDF content stream (e.g., byte value from Tj/TJ operator)
    ///
    /// # Returns
    ///
    /// Width in 1000ths of em. Returns `default_width` if the character code is not
    /// in the widths array or if widths are not available for this font.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use pdf_oxide::fonts::FontInfo;
    ///
    /// # fn example(font: &FontInfo) {
    /// // Get width for character 'A' (code 65)
    /// let width = font.get_glyph_width(65);
    /// let font_size = 12.0;
    /// let actual_width = width * font_size / 1000.0;
    /// println!("Width of 'A' at 12pt: {:.2}pt", actual_width);
    /// # }
    /// ```
    pub fn get_glyph_width(&self, char_code: u16) -> f32 {
        // For Type0 (CID) fonts, use /W array then fall back to /DW (cid_default_width).
        // F15 fix: when /DW was NOT explicitly set (has_explicit_dw=false) and the char
        // code has no entry in /W, fall through to default_width instead of returning
        // the spec-default 1000.
        // NOTE: ISO 32000-1 §9.7.4 Table 117 specifies the default for a missing /DW
        // as 1000 units. This implementation intentionally deviates from that default
        // because many non-fullwidth CID fonts omit /DW; returning 1000 for their glyphs
        // over-estimates widths and disables the gap-correction heuristic. Purely
        // fullwidth CJK fonts that omit /DW may have glyph widths under-estimated as
        // a consequence — an acceptable trade-off for the common mixed-script case.
        if self.subtype == "Type0" {
            if let Some(cid_widths) = &self.cid_widths {
                if let Some(&width) = cid_widths.get(&char_code) {
                    return width;
                }
            }
            // Only use cid_default_width if /DW was explicitly present in the font dict.
            if self.has_explicit_dw {
                return self.cid_default_width;
            }
            // Fall through to default_width — same path as simple fonts without /Widths.
        }

        // For simple fonts, use the widths array
        if let Some(widths) = &self.widths {
            if let Some(first_char) = self.first_char {
                let index = char_code as i32 - first_char as i32;
                if index >= 0 && (index as usize) < widths.len() {
                    return widths[index as usize];
                }
            }
        }
        // For standard 14 fonts without /Widths, use built-in metrics
        if let Some(w) = self.get_standard_font_width(char_code) {
            return w;
        }
        self.default_width
    }

    /// Look up width from standard 14 font metrics when /Widths array is absent
    /// or the char code falls outside the [FirstChar, LastChar] range.
    pub(super) fn get_standard_font_width(&self, char_code: u16) -> Option<f32> {
        // If a /Widths array covers this specific char code, trust it — don't override
        // with standard metrics. For chars OUTSIDE the range (including the common case
        // where space U+0020 = 32 is below a FirstChar like 66) we prefer named-font
        // metrics over the generic default_width (500), which is often too wide.
        if let Some(widths) = &self.widths {
            if let Some(first_char) = self.first_char {
                let index = char_code as i32 - first_char as i32;
                if index >= 0 && (index as usize) < widths.len() {
                    return None; // within explicit widths range – use actual width
                }
            }
        }
        // The name classification below is a pure function of `base_font`, but
        // this runs once per glyph — so it is resolved once and memoized.
        let std14 = (*self.std14_memo.get_or_init(|| self.classify_std14()))?;
        let is_bold = std14.is_bold;
        if std14.is_courier {
            return Some(600.0); // Monospace
        }
        let is_times = std14.is_times;
        let code = char_code as u8;
        self.std14_width(std14, is_times, is_bold, code)
    }

    /// Classify `base_font` against the Standard-14 set (ISO 32000-1 Annex D).
    /// `None` when the font is not one of the width-bearing standard families.
    pub(super) fn classify_std14(&self) -> Option<Std14Flags> {
        // F13 fix: use exact match against the canonical 14 standard PDF font names
        // after stripping any SUBSET+ prefix (e.g. "ABCDEF+Helvetica" → "Helvetica").
        // `contains` would incorrectly match "HelveticaCorp-Custom" as Helvetica.
        let raw_name = &self.base_font;
        let name: &str = if let Some(idx) = raw_name.find('+') {
            // Strip subset prefix: the part after '+' is the actual font name
            let suffix = &raw_name[idx + 1..];
            if suffix.is_empty() {
                raw_name
            } else {
                suffix
            }
        } else {
            raw_name
        };
        // Canonical Standard-14 font names per ISO 32000-1 Annex D.
        // "Helvetica-Oblique" is the name used by virtually all real-world PDFs;
        // the spec's canonical PostScript name is "HelveticaOblique" (no hyphen).
        // Both are accepted.
        const STANDARD_14: &[&str] = &[
            "Courier",
            "Courier-Bold",
            "Courier-BoldOblique",
            "Courier-Oblique",
            "Helvetica",
            "Helvetica-Bold",
            "Helvetica-BoldOblique",
            "Helvetica-Oblique",
            "HelveticaOblique",
            "Times-Roman",
            "Times-Bold",
            "Times-BoldItalic",
            "Times-Italic",
            "Symbol",
            "ZapfDingbats",
        ];
        if !STANDARD_14.contains(&name) {
            return None;
        }
        let is_times = name.starts_with("Times");
        let is_helvetica = name.starts_with("Helvetica");
        let is_courier = name.starts_with("Courier");

        if !is_times && !is_helvetica && !is_courier {
            return None;
        }

        Some(Std14Flags {
            is_times,
            is_courier,
            is_bold: name.contains("Bold"),
            is_bold_italic: name.contains("BoldItalic"),
            is_helvetica,
            is_italic: name.contains("Italic"),
        })
    }

    /// Standard-14 width tables, keyed off the memoized classification.
    pub(super) fn std14_width(
        &self,
        std14: Std14Flags,
        is_times: bool,
        is_bold: bool,
        code: u8,
    ) -> Option<f32> {
        // Times-Roman / Times-Bold / Times-BoldItalic standard widths (Adobe AFM metrics)
        if is_times {
            if std14.is_bold_italic {
                // Times-BoldItalic widths (Adobe Core 14 Fonts AFM).
                return Some(match code {
                    32 => 250.0,
                    33 => 389.0,
                    34 => 555.0,
                    35 => 500.0,
                    36 => 500.0,
                    37 => 833.0,
                    38 => 778.0,
                    39 => 333.0,
                    40 => 333.0,
                    41 => 333.0,
                    42 => 500.0,
                    43 => 570.0,
                    44 => 250.0,
                    45 => 333.0,
                    46 => 250.0,
                    47 => 278.0,
                    48..=57 => 500.0,
                    58 => 333.0,
                    59 => 333.0,
                    60 => 570.0,
                    61 => 570.0,
                    62 => 570.0,
                    63 => 500.0,
                    64 => 832.0,
                    65 => 667.0,
                    66 => 667.0,
                    67 => 667.0,
                    68 => 722.0,
                    69 => 667.0,
                    70 => 667.0,
                    71 => 722.0,
                    72 => 778.0,
                    73 => 389.0,
                    74 => 500.0,
                    75 => 667.0,
                    76 => 611.0,
                    77 => 889.0,
                    78 => 722.0,
                    79 => 722.0,
                    80 => 611.0,
                    81 => 722.0,
                    82 => 667.0,
                    83 => 556.0,
                    84 => 611.0,
                    85 => 722.0,
                    86 => 667.0,
                    87 => 889.0,
                    88 => 667.0,
                    89 => 611.0,
                    90 => 611.0,
                    91 => 333.0,
                    92 => 278.0,
                    93 => 333.0,
                    94 => 570.0,
                    95 => 500.0,
                    97 => 500.0,
                    98 => 500.0,
                    99 => 444.0,
                    100 => 500.0,
                    101 => 444.0,
                    102 => 333.0,
                    103 => 500.0,
                    104 => 556.0,
                    105 => 278.0,
                    106 => 278.0,
                    107 => 500.0,
                    108 => 278.0,
                    109 => 778.0,
                    110 => 556.0,
                    111 => 500.0,
                    112 => 500.0,
                    113 => 500.0,
                    114 => 389.0,
                    115 => 389.0,
                    116 => 278.0,
                    117 => 556.0,
                    118 => 444.0,
                    119 => 667.0,
                    120 => 500.0,
                    121 => 444.0,
                    122 => 389.0,
                    _ => return None,
                });
            }
            if is_bold {
                // Times-Bold widths (Adobe Core 14 Fonts AFM).
                return Some(match code {
                    32 => 250.0,
                    33 => 333.0,
                    34 => 555.0,
                    35 => 500.0,
                    36 => 500.0,
                    37 => 1000.0,
                    38 => 833.0,
                    39 => 333.0,
                    40 => 333.0,
                    41 => 333.0,
                    42 => 500.0,
                    43 => 570.0,
                    44 => 250.0,
                    45 => 333.0,
                    46 => 250.0,
                    47 => 278.0,
                    48..=57 => 500.0,
                    58 => 333.0,
                    59 => 333.0,
                    60 => 570.0,
                    61 => 570.0,
                    62 => 570.0,
                    63 => 500.0,
                    64 => 930.0,
                    65 => 722.0,
                    66 => 667.0,
                    67 => 722.0,
                    68 => 722.0,
                    69 => 667.0,
                    70 => 611.0,
                    71 => 778.0,
                    72 => 778.0,
                    73 => 389.0,
                    74 => 500.0,
                    75 => 778.0,
                    76 => 667.0,
                    77 => 944.0,
                    78 => 722.0,
                    79 => 778.0,
                    80 => 611.0,
                    81 => 778.0,
                    82 => 722.0,
                    83 => 556.0,
                    84 => 667.0,
                    85 => 722.0,
                    86 => 722.0,
                    87 => 1000.0,
                    88 => 722.0,
                    89 => 722.0,
                    90 => 667.0,
                    91 => 333.0,
                    92 => 278.0,
                    93 => 333.0,
                    94 => 581.0,
                    95 => 500.0,
                    97 => 500.0,
                    98 => 556.0,
                    99 => 444.0,
                    100 => 556.0,
                    101 => 444.0,
                    102 => 333.0,
                    103 => 500.0,
                    104 => 556.0,
                    105 => 278.0,
                    106 => 333.0,
                    107 => 556.0,
                    108 => 278.0,
                    109 => 833.0,
                    110 => 556.0,
                    111 => 500.0,
                    112 => 556.0,
                    113 => 556.0,
                    114 => 444.0,
                    115 => 389.0,
                    116 => 333.0,
                    117 => 556.0,
                    118 => 500.0,
                    119 => 722.0,
                    120 => 500.0,
                    121 => 500.0,
                    122 => 444.0,
                    _ => return None,
                });
            }
            if std14.is_italic {
                // Times-Italic widths (Adobe Core 14 Fonts AFM).
                return Some(match code {
                    32 => 250.0,
                    33 => 333.0,
                    34 => 420.0,
                    35 => 500.0,
                    36 => 500.0,
                    37 => 833.0,
                    38 => 778.0,
                    39 => 333.0,
                    40 => 333.0,
                    41 => 333.0,
                    42 => 500.0,
                    43 => 675.0,
                    44 => 250.0,
                    45 => 333.0,
                    46 => 250.0,
                    47 => 278.0,
                    48..=57 => 500.0,
                    58 => 333.0,
                    59 => 333.0,
                    60 => 675.0,
                    61 => 675.0,
                    62 => 675.0,
                    63 => 500.0,
                    64 => 920.0,
                    65 => 611.0,
                    66 => 611.0,
                    67 => 667.0,
                    68 => 722.0,
                    69 => 611.0,
                    70 => 611.0,
                    71 => 722.0,
                    72 => 722.0,
                    73 => 333.0,
                    74 => 444.0,
                    75 => 667.0,
                    76 => 556.0,
                    77 => 833.0,
                    78 => 667.0,
                    79 => 722.0,
                    80 => 611.0,
                    81 => 722.0,
                    82 => 611.0,
                    83 => 500.0,
                    84 => 556.0,
                    85 => 722.0,
                    86 => 611.0,
                    87 => 833.0,
                    88 => 611.0,
                    89 => 556.0,
                    90 => 556.0,
                    91 => 389.0,
                    92 => 278.0,
                    93 => 389.0,
                    94 => 422.0,
                    95 => 500.0,
                    97 => 500.0,
                    98 => 500.0,
                    99 => 444.0,
                    100 => 500.0,
                    101 => 444.0,
                    102 => 278.0,
                    103 => 500.0,
                    104 => 500.0,
                    105 => 278.0,
                    106 => 278.0,
                    107 => 444.0,
                    108 => 278.0,
                    109 => 722.0,
                    110 => 500.0,
                    111 => 500.0,
                    112 => 500.0,
                    113 => 500.0,
                    114 => 389.0,
                    115 => 389.0,
                    116 => 278.0,
                    117 => 500.0,
                    118 => 444.0,
                    119 => 667.0,
                    120 => 444.0,
                    121 => 444.0,
                    122 => 389.0,
                    _ => return None,
                });
            }
            return Some(match code {
                32 => 250.0,
                33 => 333.0,
                34 => 408.0,
                35 => 500.0,
                36 => 500.0,
                37 => 833.0,
                38 => 778.0,
                39 => 333.0,
                40 => 333.0,
                41 => 333.0,
                42 => 500.0,
                43 => 564.0,
                44 => 250.0,
                45 => 333.0,
                46 => 250.0,
                47 => 278.0,
                48 => 500.0,
                49 => 500.0,
                50 => 500.0,
                51 => 500.0,
                52 => 500.0,
                53 => 500.0,
                54 => 500.0,
                55 => 500.0,
                56 => 500.0,
                57 => 500.0,
                58 => 278.0,
                59 => 278.0,
                60 => 564.0,
                61 => 564.0,
                62 => 564.0,
                63 => 444.0,
                64 => 921.0,
                65 => 722.0,
                66 => 667.0,
                67 => 667.0,
                68 => 722.0,
                69 => 611.0,
                70 => 556.0,
                71 => 722.0,
                72 => 722.0,
                73 => 333.0,
                74 => 389.0,
                75 => 722.0,
                76 => 611.0,
                77 => 889.0,
                78 => 722.0,
                79 => 722.0,
                80 => 556.0,
                81 => 722.0,
                82 => 667.0,
                83 => 556.0,
                84 => 611.0,
                85 => 722.0,
                86 => 722.0,
                87 => 944.0,
                88 => 722.0,
                89 => 722.0,
                90 => 611.0,
                91 => 333.0,
                92 => 278.0,
                93 => 333.0,
                97 => 444.0,
                98 => 500.0,
                99 => 444.0,
                100 => 500.0,
                101 => 444.0,
                102 => 333.0,
                103 => 500.0,
                104 => 500.0,
                105 => 278.0,
                106 => 278.0,
                107 => 500.0,
                108 => 278.0,
                109 => 778.0,
                110 => 500.0,
                111 => 500.0,
                112 => 500.0,
                113 => 500.0,
                114 => 333.0,
                115 => 389.0,
                116 => 278.0,
                117 => 500.0,
                118 => 500.0,
                119 => 722.0,
                120 => 500.0,
                121 => 500.0,
                122 => 444.0,
                _ => return None,
            });
        }

        // Helvetica / Helvetica-Bold standard widths (Adobe AFM metrics)
        if std14.is_helvetica {
            if is_bold {
                // Helvetica-Bold / Helvetica-BoldOblique widths (Adobe Core 14 Fonts AFM).
                return Some(match code {
                    32 => 278.0,
                    33 => 333.0,
                    34 => 474.0,
                    44 => 278.0,
                    45 => 333.0,
                    46 => 278.0,
                    47 => 278.0,
                    48..=57 => 556.0,
                    58 => 333.0,
                    59 => 333.0,
                    65 => 722.0,
                    66 => 722.0,
                    67 => 722.0,
                    68 => 722.0,
                    69 => 667.0,
                    70 => 611.0,
                    71 => 778.0,
                    72 => 722.0,
                    73 => 278.0,
                    74 => 556.0,
                    75 => 722.0,
                    76 => 611.0,
                    77 => 833.0,
                    78 => 722.0,
                    79 => 778.0,
                    80 => 667.0,
                    81 => 778.0,
                    82 => 722.0,
                    83 => 667.0,
                    84 => 611.0,
                    85 => 722.0,
                    86 => 667.0,
                    87 => 944.0,
                    88 => 667.0,
                    89 => 667.0,
                    90 => 611.0,
                    97 => 556.0,
                    98 => 611.0,
                    99 => 556.0,
                    100 => 611.0,
                    101 => 556.0,
                    102 => 333.0,
                    103 => 611.0,
                    104 => 611.0,
                    105 => 278.0,
                    106 => 278.0,
                    107 => 556.0,
                    108 => 278.0,
                    109 => 889.0,
                    110 => 611.0,
                    111 => 611.0,
                    112 => 611.0,
                    113 => 611.0,
                    114 => 389.0,
                    115 => 556.0,
                    116 => 333.0,
                    117 => 611.0,
                    118 => 556.0,
                    119 => 778.0,
                    120 => 556.0,
                    121 => 556.0,
                    122 => 500.0,
                    _ => return None,
                });
            }
            return Some(match code {
                32 => 278.0,
                33 => 278.0,
                34 => 355.0,
                44 => 278.0,
                45 => 333.0,
                46 => 278.0,
                47 => 278.0,
                48..=57 => 556.0, // digits
                58 => 278.0,
                59 => 278.0,
                65 => 667.0,
                66 => 667.0,
                67 => 722.0,
                68 => 722.0,
                69 => 667.0,
                70 => 611.0,
                71 => 778.0,
                72 => 722.0,
                73 => 278.0,
                74 => 500.0,
                75 => 667.0,
                76 => 556.0,
                77 => 833.0,
                78 => 722.0,
                79 => 778.0,
                80 => 667.0,
                81 => 778.0,
                82 => 722.0,
                83 => 667.0,
                84 => 611.0,
                85 => 722.0,
                86 => 667.0,
                87 => 944.0,
                88 => 667.0,
                89 => 667.0,
                90 => 611.0,
                97 => 556.0,
                98 => 556.0,
                99 => 500.0,
                100 => 556.0,
                101 => 556.0,
                102 => 278.0,
                103 => 556.0,
                104 => 556.0,
                105 => 222.0,
                106 => 222.0,
                107 => 500.0,
                108 => 222.0,
                109 => 833.0,
                110 => 556.0,
                111 => 556.0,
                112 => 556.0,
                113 => 556.0,
                114 => 333.0,
                115 => 500.0,
                116 => 278.0,
                117 => 556.0,
                118 => 500.0,
                119 => 722.0,
                120 => 500.0,
                121 => 500.0,
                122 => 444.0,
                _ => return None,
            });
        }
        None
    }

    /// Get the width of the space glyph (U+0020) in font units.
    ///
    /// Returns the width in 1000ths of em per PDF spec Section 9.7.4.
    /// Used for font-aware spacing threshold calculations.
    ///
    /// Per PDF Spec Section 9.4.4, word spacing should be based on actual font metrics
    /// rather than fixed ratios. This method returns the actual space glyph width,
    /// which is used to compute adaptive TJ offset thresholds that account for
    /// different font sizes and families.
    ///
    /// # Returns
    ///
    /// The width of the space character (code 0x20) in 1000ths of em. When no
    /// real space glyph is defined — a simple font with a near-zero 0x20, or a
    /// CID font with no explicit /W entry for 0x20 — returns the 0.25 em (250)
    /// typographic default rather than the font's (often much wider) /DW.
    pub fn get_space_glyph_width(&self) -> f32 {
        // The space advance feeds the caller's geometric word-gap threshold
        // (threshold = space_width × ratio); a value that is not actually the
        // space glyph's advance skews that threshold and mis-detects word
        // boundaries.
        //
        // Type0 (CID-keyed) fonts under Identity-H/V — the encoding of nearly
        // every embedded subset — map character code 0x20 to CID 32, an
        // arbitrary font-internal glyph, NOT the space. The space glyph, if
        // present, lives at a CID reached through the font's CMap / ToUnicode,
        // never at code 0x20 (ISO 32000-2 §9.7.5.2, §9.10.2). So `cid_widths`
        // keyed by 0x20 is the advance of whatever glyph sits at CID 32 —
        // frequently ~0.5 em+ (TimesNewRomanPSMT reports 563) — and feeding it
        // into the threshold makes it so wide that real justified word gaps
        // fall below it and adjacent words glue together ("All rights reserved"
        // -> "Allrightsreserved", #803). For Identity-encoded Type0 fonts,
        // ignore code 0x20 entirely and use the 0.25 em typographic default.
        if self.subtype == "Type0" {
            if matches!(self.encoding, Encoding::Identity) {
                return 250.0;
            }
            // Non-Identity predefined CMap (e.g. 90ms-RKSJ-H): code 0x20 can map
            // to a real space CID, so an explicit /W entry is meaningful.
            return match self.cid_widths.as_ref().and_then(|w| w.get(&0x20)) {
                Some(&w) if w >= 50.0 => w,
                _ => 250.0,
            };
        }
        // Space character is always code 0x20 (32) in a simple font.
        let w = self.get_glyph_width(0x20);
        // Many simple subset fonts (notably shaped Arabic from Chrome /
        // browser print) omit a glyph for code 0x20 entirely, so this returns
        // ~0. A zero width collapses the threshold to 0, so *every* inter-glyph
        // kerning gap is read as a word boundary and cursive Arabic words
        // shatter into single letters. Fall back to a typographic
        // default of 0.25 em (250 font units) — the same value
        // `should_insert_space` uses when the font is absent.
        if w < 50.0 {
            250.0
        } else {
            w
        }
    }
}
