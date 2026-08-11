use super::*;

/// Byte grouping mode for CID font character code decoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ByteMode {
    /// Single-byte codes (simple fonts, some predefined CMaps)
    OneByte,
    /// Always 2-byte codes (Identity-H/V, UCS2)
    TwoByte,
    /// Shift-JIS variable-width (1 or 2 bytes depending on lead byte)
    ShiftJIS,
}

/// Get byte grouping mode for a font.
pub(super) fn get_byte_mode(font: Option<&crate::fonts::FontInfo>) -> ByteMode {
    if let Some(font) = font {
        if font.subtype == "Type0" {
            match &font.encoding {
                crate::fonts::Encoding::Identity => ByteMode::TwoByte,
                crate::fonts::Encoding::Standard(name) => {
                    if (name.contains("Identity") && !name.contains("OneByteIdentity"))
                        || name.contains("UCS2")
                        || name.contains("UTF16")
                    {
                        ByteMode::TwoByte
                    } else if name.contains("RKSJ") {
                        ByteMode::ShiftJIS
                    } else if name.contains("EUC")
                        || name.contains("GBK")
                        || name.contains("GBpc")
                        || name.contains("GB-")
                        || name.contains("CNS")
                        || name.contains("B5")
                        || name.contains("KSC")
                        || name.contains("KSCms")
                    {
                        ByteMode::TwoByte
                    } else {
                        ByteMode::OneByte
                    }
                }
                _ => ByteMode::OneByte,
            }
        } else {
            ByteMode::OneByte
        }
    } else {
        ByteMode::OneByte
    }
}

/// Iterator over characters in a PDF string based on font encoding.
pub(super) struct TextCharIter<'a> {
    bytes: &'a [u8],
    byte_mode: ByteMode,
    index: usize,
}

impl<'a> TextCharIter<'a> {
    pub(super) fn new(bytes: &'a [u8], font: Option<&crate::fonts::FontInfo>) -> Self {
        Self {
            bytes,
            byte_mode: get_byte_mode(font),
            index: 0,
        }
    }
}

impl<'a> Iterator for TextCharIter<'a> {
    type Item = (u16, usize); // (char_code, bytes_consumed)

    fn next(&mut self) -> Option<Self::Item> {
        if self.index >= self.bytes.len() {
            return None;
        }

        let (char_code, bytes_consumed) = match self.byte_mode {
            ByteMode::TwoByte if self.index + 1 < self.bytes.len() => (
                ((self.bytes[self.index] as u16) << 8) | (self.bytes[self.index + 1] as u16),
                2,
            ),
            ByteMode::ShiftJIS => {
                let b = self.bytes[self.index];
                let is_lead = (0x81..=0x9F).contains(&b) || (0xE0..=0xFC).contains(&b);
                if is_lead && self.index + 1 < self.bytes.len() {
                    (((b as u16) << 8) | (self.bytes[self.index + 1] as u16), 2)
                } else {
                    (b as u16, 1)
                }
            }
            _ => (self.bytes[self.index] as u16, 1),
        };

        self.index += bytes_consumed;
        Some((char_code, bytes_consumed))
    }
}

/// Fallback function to map common character codes to Unicode when ToUnicode CMap fails.
pub(super) fn fallback_char_to_unicode(char_code: u32) -> String {
    match char_code {
        0x2014 => "—".to_string(),
        0x2013 => "–".to_string(),
        0x2018 => "\u{2018}".to_string(),
        0x2019 => "\u{2019}".to_string(),
        0x201C => "\u{201C}".to_string(),
        0x201D => "\u{201D}".to_string(),
        0x2022 => "•".to_string(),
        0x2026 => "…".to_string(),
        0x00B0 => "°".to_string(),
        0x00B1 => "±".to_string(),
        0x00D7 => "×".to_string(),
        0x00F7 => "÷".to_string(),
        0x2202 => "∂".to_string(),
        0x2207 => "∇".to_string(),
        0x220F => "∏".to_string(),
        0x2211 => "∑".to_string(),
        0x221A => "√".to_string(),
        0x221E => "∞".to_string(),
        0x2260 => "≠".to_string(),
        0x2261 => "≡".to_string(),
        0x2264 => "≤".to_string(),
        0x2265 => "≥".to_string(),
        code => {
            if let Some(ch) = char::from_u32(code) {
                ch.to_string()
            } else {
                "\u{FFFD}".to_string()
            }
        }
    }
}

/// Compute the PDF-spec text advance for `bytes` without painting,
/// returning the scalar magnitude along the active writing axis.
///
/// Mirrors the advance math in [`TextRasterizer::render_unicode_text`] but
/// without any glyph outline work. Per ISO 32000-1 §9.4.4:
///
/// - Horizontal mode (`gs.text_wmode == 0`):
///   `tx = ((w0 * Tfs) + Tc + Tw) * Th`
/// - Vertical mode (`gs.text_wmode == 1`):
///   `ty = (w1y * Tfs) + Tc + Tw`
///
/// `w0` / `w1y` are in 1000ths of an em, `Tfs` is the font size, `Tc` is
/// `char_space`, `Tw` is `word_space` (applied at the space CID 0x20), and
/// `Th` is `horizontal_scaling / 100` (used in horizontal mode only — per
/// §9.3.4 horizontal scaling is along the writing direction).
///
/// When no font metrics are available we fall back to a half-em estimate per
/// character — same constant `render_text_fallback` uses for the visible path,
/// so the suppressed branch stays consistent with the painted branch.
pub(super) fn measure_text_bytes(
    bytes: &[u8],
    gs: &GraphicsState,
    font_info: Option<&crate::fonts::FontInfo>,
) -> f32 {
    let font_size = gs.font_size;
    let h_scale = gs.horizontal_scaling / 100.0;
    let wmode = gs.text_wmode;
    let mut advance: f32 = 0.0;

    if let Some(font) = font_info {
        for (char_code, _) in TextCharIter::new(bytes, Some(font)) {
            // Per ISO 32000-1 §9.4.4 the advance formula differs by writing
            // mode:
            //   horizontal: tx = ((w0 * Tfs) + Tc + Tw) * Th
            //   vertical:   ty = (w1y * Tfs) + Tc + Tw       (NO Th)
            // Tz is defined as glyph stretching along the *horizontal*
            // direction only (§9.3.4); it does not scale vertical w1y or
            // vertical Tc / Tw.
            if wmode == 0 {
                let glyph_adv = font.get_glyph_width(char_code) * font_size / 1000.0;
                advance += (glyph_adv + gs.char_space) * h_scale;
                if char_code == 0x20 {
                    advance += gs.word_space * h_scale;
                }
            } else {
                let w1y = font.get_vertical_metrics(char_code).w1y;
                let glyph_adv = w1y * font_size / 1000.0;
                advance += glyph_adv + gs.char_space;
                if char_code == 0x20 {
                    advance += gs.word_space;
                }
            }
        }
    } else {
        // No font info — half-em estimate per byte. Match the wmode-aware
        // arm above by omitting h_scale in vertical mode.
        let char_width = font_size * 0.6;
        for &b in bytes {
            if wmode == 0 {
                advance += (char_width + gs.char_space) * h_scale;
                if b == 0x20 {
                    advance += gs.word_space * h_scale;
                }
            } else {
                advance += char_width + gs.char_space;
                if b == 0x20 {
                    advance += gs.word_space;
                }
            }
        }
    }
    advance
}
