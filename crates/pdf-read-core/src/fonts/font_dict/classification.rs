use super::*;

impl FontInfo {
    /// Determine the font weight using a comprehensive cascade of PDF spec methods.
    ///
    /// Priority order per PDF Spec ISO 32000-1:2008:
    /// 1. FontWeight field from FontDescriptor (Table 122) - MOST RELIABLE
    /// 2. ForceBold flag (bit 19) from Flags field (Table 123)
    /// 3. Font name heuristics (fallback for legacy PDFs)
    /// 4. StemV analysis (stem thickness correlates with weight)
    ///
    /// # Returns
    ///
    /// FontWeight enum value (Thin to Black scale)
    ///
    /// # PDF Spec References
    ///
    /// - Table 122 (page 456): FontWeight values 100-900
    /// - Table 123 (page 457): ForceBold flag at bit 19 (0x80000)
    /// - Section 9.6.2: StemV field interpretation
    pub fn get_font_weight(&self) -> FontWeight {
        *self.weight_memo.get_or_init(|| self.compute_font_weight())
    }

    /// Uncached [`Self::get_font_weight`] body. Everything it reads is fixed at
    /// font-load time, so `weight_memo` can hold the answer for the font's life.
    pub(super) fn compute_font_weight(&self) -> FontWeight {
        // ==================================================================================
        // PRIORITY 1: FontWeight Field (PDF Spec Table 122)
        // ==================================================================================
        // Most reliable method. If present, use directly.
        if let Some(weight_value) = self.font_weight {
            return FontWeight::from_pdf_value(weight_value);
        }

        // ==================================================================================
        // PRIORITY 2: ForceBold Flag (PDF Spec Table 123, Bit 19)
        // ==================================================================================
        // If ForceBold flag is set, font is explicitly bold.
        // Bit 19 = 0x80000 (524288 decimal)
        if let Some(flags_value) = self.flags {
            const FORCE_BOLD_BIT: i32 = 0x80000; // Bit 19 = 524288
            if (flags_value & FORCE_BOLD_BIT) != 0 {
                log::debug!(
                    "Font '{}': ForceBold flag set (bit 19) → Bold",
                    self.base_font
                );
                return FontWeight::Bold;
            }
        }

        // ==================================================================================
        // PRIORITY 3: Font Name Heuristics
        // ==================================================================================
        // Fallback for fonts without FontDescriptor or with missing fields.
        // Checks for bold-indicating keywords in the font name.
        let name_lower = self.base_font.to_lowercase();

        // Check for explicit weight keywords in order of strength
        if name_lower.contains("black") || name_lower.contains("heavy") {
            return FontWeight::Black; // 900
        }
        if name_lower.contains("extrabold") || name_lower.contains("ultrabold") {
            return FontWeight::ExtraBold; // 800
        }
        if name_lower.contains("bold") {
            // Distinguish between "SemiBold" and "Bold"
            if name_lower.contains("semibold") || name_lower.contains("demibold") {
                return FontWeight::SemiBold; // 600
            }
            return FontWeight::Bold; // 700
        }
        if name_lower.contains("medium") {
            return FontWeight::Medium; // 500
        }
        if name_lower.contains("light") {
            if name_lower.contains("extralight") || name_lower.contains("ultralight") {
                return FontWeight::ExtraLight; // 200
            }
            return FontWeight::Light; // 300
        }
        if name_lower.contains("thin") {
            return FontWeight::Thin; // 100
        }

        // ==================================================================================
        // PRIORITY 4: StemV Analysis (EXPERIMENTAL)
        // ==================================================================================
        // StemV measures vertical stem thickness. Empirically:
        // - StemV > 110: Usually bold (700+)
        // - StemV 80-110: Medium (500-600)
        // - StemV < 80: Normal or lighter (400-)
        //
        // NOTE: This is a heuristic and may not be reliable for all fonts.
        // PDF spec does not mandate this correlation.
        if let Some(stem_v) = self.stem_v {
            log::debug!(
                "Font '{}': Using StemV analysis (StemV={})",
                self.base_font,
                stem_v
            );

            if stem_v > 110.0 {
                return FontWeight::Bold; // 700
            } else if stem_v >= 80.0 {
                return FontWeight::Medium; // 500
            }
            // If StemV < 80, continue to default (Normal)
        }

        // ==================================================================================
        // DEFAULT: Normal Weight (400)
        // ==================================================================================
        // If no other method yields a weight, assume normal.
        FontWeight::Normal
    }

    /// Check if this font is bold (convenience method).
    ///
    /// Returns true if font weight is SemiBold (600) or higher.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// if font.is_bold() {
    ///     // Apply bold markdown formatting
    /// }
    /// ```
    pub fn is_bold(&self) -> bool {
        self.get_font_weight().is_bold()
    }

    /// Return true when this font's per-glyph widths come from the PDF's
    /// `/Widths` array (for simple fonts) or `/W` array (for Type0/CID
    /// fonts), rather than from the generic 500/550/600-thousandths-of-em
    /// fallback that `FontInfo::new` uses when neither is present.
    ///
    /// Callers use this to decide whether `byte_to_width_table` is
    /// trustworthy: when it returns false, every glyph reports the same
    /// fallback advance, so bounding-box widths computed from those
    /// advances systematically over- or under-estimate the visible text
    /// extent. On affected PDFs that collapses the real gap between
    /// adjacent `Tj`-positioned words, gluing words together in
    /// `extract_text` output even though the PDF itself places them on
    /// distinct positions. See issue #328.
    pub fn has_explicit_widths(&self) -> bool {
        // F14 fix: return true only when the font actually has explicit width data.
        // Previously returned true for ALL Type0 fonts, which disabled gap-correction
        // for Type0 fonts with no /W or /DW — exactly the fonts that need correction.
        // Now: true when /Widths is present (simple fonts), or when /W has entries
        // (CID fonts), or when /DW was explicitly set in the CIDFont dictionary.
        self.widths.is_some() || self.cid_widths.is_some() || self.has_explicit_dw
    }

    /// Check if this font is likely italic based on the font name.
    ///
    /// This is a heuristic check looking for "Italic" or "Oblique" in the font name.
    pub fn is_italic(&self) -> bool {
        *self.italic_memo.get_or_init(|| {
            let name_lower = self.base_font.to_lowercase();
            name_lower.contains("italic") || name_lower.contains("oblique")
        })
    }

    /// Check if this is a symbolic font based on FontDescriptor flags.
    ///
    /// Symbolic fonts (bit 3 set in /Flags) contain glyphs outside the Adobe standard
    /// Latin character set. For symbolic fonts, the PDF spec requires ignoring any
    /// Encoding entry and using direct character code mapping to the font's built-in encoding.
    ///
    /// Common symbolic fonts: Symbol, ZapfDingbats
    ///
    /// PDF Spec: ISO 32000-1:2008, Table 5.20 - Font descriptor flags
    /// Bit 3: Symbolic - Font contains glyphs outside Adobe standard Latin character set
    /// Bit 6: Nonsymbolic - Font uses Adobe standard Latin character set (mutually exclusive with bit 3)
    pub fn is_symbolic(&self) -> bool {
        // Priority 1: Check FontDescriptor /Flags bit 3
        if let Some(flags_value) = self.flags {
            // Bit 3 = 0x04 (1 << 2, since bits are numbered starting at 1 in PDF spec)
            const SYMBOLIC_BIT: i32 = 1 << 2; // Bit 3
            return (flags_value & SYMBOLIC_BIT) != 0;
        }

        // Priority 2: Fallback to font name heuristic
        let name_lower = self.base_font.to_lowercase();
        name_lower.contains("symbol")
            || name_lower.contains("zapf")
            || name_lower.contains("dingbat")
    }

    /// Get character from encoding (custom or standard).
    ///
    /// Custom encoding support
    ///
    /// This method normalizes a raw character code through the font's encoding,
    /// converting it to the actual Unicode character. This ensures word boundary
    /// detection works on real characters, not raw byte codes.
    ///
    /// Per PDF Spec ISO 32000-1:2008, Section 9.6.6:
    /// - Custom encodings with /Differences override standard encodings
    /// - Standard encodings have well-defined mappings
    /// - Identity encoding passes codes through as-is
    ///
    /// # Arguments
    ///
    /// * `code` - The raw byte value from the PDF content stream
    ///
    /// # Returns
    ///
    /// The normalized Unicode character, or None if no mapping exists
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use pdf_oxide::fonts::FontInfo;
    ///
    /// let font_info = /* ... load font ... */;
    /// if let Some(ch) = font_info.get_encoded_char(0x64) {
    ///     println!("Code 0x64 maps to: {}", ch);
    /// }
    /// ```
    pub fn get_encoded_char(&self, code: u8) -> Option<char> {
        match &self.encoding {
            Encoding::Custom(mappings) => {
                // Custom encoding: use explicit character mappings
                mappings.get(&code).copied()
            }
            Encoding::Standard(_encoding_name) => {
                // Standard encoding: for now, assume ToUnicode CMap handles this
                // If we need explicit standard encoding tables, add them here
                // For basic ASCII range, we can pass through
                if code < 128 {
                    Some(code as char)
                } else {
                    None
                }
            }
            Encoding::Identity => {
                // Identity encoding: code == Unicode (for CID fonts)
                // For single-byte codes, treat as Unicode
                if code < 128 {
                    Some(code as char)
                } else {
                    None
                }
            }
        }
    }

    /// Check if font has custom encoding.
    ///
    /// Custom encoding support
    ///
    /// Returns true if the font uses a custom encoding with /Differences array,
    /// which overrides standard encoding for specific character codes.
    ///
    /// # Returns
    ///
    /// true if the font has a custom encoding, false otherwise
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use pdf_oxide::fonts::FontInfo;
    ///
    /// let font_info = /* ... load font ... */;
    /// if font_info.has_custom_encoding() {
    ///     println!("Font uses custom encoding");
    /// }
    /// ```
    pub fn has_custom_encoding(&self) -> bool {
        matches!(self.encoding, Encoding::Custom(_))
    }
}
