//! Text rasterizer - renders PDF text using tiny-skia.
//!
//! Text rendering in PDF is complex because:
//! - Fonts may be embedded or use standard PDF fonts
//! - Character encoding varies (identity-H, MacRoman, custom ToUnicode, etc.)
#![allow(clippy::collapsible_if, clippy::vec_box)]
//! - Glyph positioning is explicit via TJ arrays
//!
//! This module provides a text rendering implementation that:
//! - Uses system fonts as fallback when embedded fonts aren't available
//! - Renders text using harfrust for shaping and tiny-skia for drawing glyph paths

use super::create_fill_paint;
use crate::content::operators::TextElement;
use crate::content::GraphicsState;
use crate::document::PdfDocument;
use crate::error::{Error, Result};
use crate::object::Object;
use std::collections::HashMap;
use std::sync::Arc;

use tiny_skia::{Paint, PathBuilder, Pixmap, Transform};
use ttf_parser::OutlineBuilder;

mod cid;
mod encoding;
mod fonts;
mod outline;
mod unicode;

use encoding::{fallback_char_to_unicode, measure_text_bytes, TextCharIter};
use fonts::{classify_embedded_font, cmap_byte_to_gid, get_cjk_fallback_cached, system_fontdb};
use outline::SkiaOutlineBuilder;

/// Rasterizer for PDF text operations.
pub struct TextRasterizer {
    /// Font database for system font fallback.
    ///
    /// Shared across rasterizers via a process-wide `OnceLock` cache so
    /// we don't re-scan the system font directories on every new
    /// `PageRenderer`. See the `SYSTEM_FONTDB` docstring for the
    /// measurement that motivated the switch.
    fontdb: std::sync::Arc<fontdb::Database>,
}

impl TextRasterizer {
    /// Create a new text rasterizer using the cached system font database.
    pub fn new() -> Self {
        Self {
            fontdb: system_fontdb(),
        }
    }

    /// Construct with a caller-supplied font database. Bypasses the
    /// process-wide cache — useful for tests or callers that need to
    /// pre-populate the database with non-system fonts.
    #[allow(dead_code)]
    pub fn with_fontdb(fontdb: std::sync::Arc<fontdb::Database>) -> Self {
        Self { fontdb }
    }

    /// Render a text string (Tj operator).
    /// Returns the total horizontal advance in PDF points.
    ///
    /// `color_override` carries the resolution-pipeline output: the
    /// fill RGBA replaces the value `gs` would supply when present, so
    /// the operator arm doesn't have to clone `gs` purely to splice a
    /// colour. Stroke override is accepted for forward compatibility —
    /// the text rasteriser does not currently paint stroked glyphs, so
    /// the stroke channel is recorded but not yet observable on the
    /// pixmap.
    #[allow(unused_variables)]
    pub fn render_text(
        &self,
        pixmap: &mut Pixmap,
        text: &[u8],
        base_transform: Transform,
        gs: &GraphicsState,
        color_override: Option<&crate::rendering::page_renderer::ResolvedColors>,
        _resources: &Object,
        doc: &PdfDocument,
        clip_mask: Option<&tiny_skia::Mask>,
        font_cache: &HashMap<String, Arc<crate::fonts::FontInfo>>,
    ) -> Result<f32> {
        // Get font info from cache
        let font_info = if let Some(font_name) = &gs.font_name {
            font_cache.get(font_name).cloned()
        } else {
            None
        };

        // Convert raw PDF bytes to Unicode string using font encoding
        let unicode_text = self.decode_text_to_unicode(text, font_info.as_deref());
        log::debug!("Decoded text: '{}' (font={:?})", unicode_text, gs.font_name);

        // Create paint from fill color, then apply the pipeline-resolved
        // override when present. `create_fill_paint` reads gs.fill_*
        // unconditionally; the override stamp afterwards is the only
        // place the resolved RGBA needs to land for visible-glyph paint.
        let mut paint = create_fill_paint(gs, "Normal");
        if let Some(overrides) = color_override {
            if let Some((r, g, b, a)) = overrides.fill {
                paint.set_color(
                    tiny_skia::Color::from_rgba(r, g, b, a).unwrap_or(tiny_skia::Color::BLACK),
                );
            }
        }
        // Text rendering mode 3 = invisible text (searchable OCR layers).
        // Mode 7 = add-to-clip-path only, with NO painting (ISO 32000-1
        // §9.3.6); it previously fell through and painted glyphs visibly. The
        // clip-path accumulation itself (modes 4–7) is not yet applied, but
        // mode-7 glyphs must at minimum not paint. (WS1.5)
        if gs.render_mode == 3 || gs.render_mode == 7 {
            paint.set_color(tiny_skia::Color::from_rgba(0.0, 0.0, 0.0, 0.0).unwrap());
        }

        // Find and load font - prioritize embedded font data
        let pdf_font_name = gs.font_name.as_deref().unwrap_or("Helvetica");
        let font_data_and_index: Option<(Option<fontdb::ID>, Arc<Vec<u8>>, u32, bool)> =
            if let Some(ref info) = font_info {
                if let Some(ref embedded) = info.embedded_font_data {
                    // Simple (non-Type0) TrueType subsets whose sole cmap subtable
                    // is a byte-indexed table must be rendered by feeding the raw
                    // PDF content bytes to the embedded cmap directly — the PDF
                    // byte is the cmap input under the font's declared encoding
                    // (ISO 32000-1 §9.6.6.4). Unicode shaping against these fonts
                    // is unreliable: even if a space or punctuation happens to
                    // share a codepoint with a cmap key, shaping for letters
                    // resolves to .notdef and the system-font fallback picks up
                    // unrelated glyphs. Bypass the Unicode shaping path entirely
                    // for this subtype so the byte→GID route is taken for every
                    // `Tj` / `TJ` call, not just the ones whose decoded Unicode
                    // happens to miss the cmap.
                    // Classify the embedded font's cmap tables. Computed
                    // locally on every call — a cheap zero-copy `ttf_parser`
                    // probe; the process-wide memoisation was removed as
                    // unsound under concurrency (issue #505).
                    let (is_byte_indexed, has_unicode_cmap) = classify_embedded_font(embedded);
                    if info.subtype != "Type0" && is_byte_indexed {
                        log::debug!(
                        "Using embedded font '{}' with byte-indexed cmap (simple TrueType subset)",
                        info.base_font
                    );
                        return self.render_cid_direct(
                            pixmap,
                            text,
                            info,
                            embedded,
                            0,
                            &paint,
                            base_transform,
                            gs,
                            clip_mask,
                        );
                    }

                    if has_unicode_cmap {
                        log::debug!("Using embedded font data for '{}'", info.base_font);
                        Some((None, Arc::clone(embedded), 0, false))
                    } else if info.subtype == "Type0"
                        && info.cid_to_gid_map.is_some()
                        && info.cid_font_type.as_deref() == Some("CIDFontType2")
                    {
                        // CIDFontType2 (TrueType) with CIDToGIDMap — use direct GID rendering.
                        log::debug!(
                            "Using embedded font '{}' with CIDToGIDMap (CIDFontType2)",
                            info.base_font
                        );
                        Some((None, Arc::clone(embedded), 0, true))
                    } else if info.cff_gid_map.is_some()
                        || (info.subtype == "Type0"
                            && info.cid_font_type.as_deref() == Some("CIDFontType0"))
                    {
                        // CFF font — use direct GID rendering.
                        //
                        // For simple (non-Type0) CFF fonts the `cff_gid_map` is
                        // built at load time by
                        // [`crate::fonts::cff_encoding::parse_cff_gid_mapping_with_pdf_encoding`],
                        // which uses the PDF font dictionary's `/Encoding`
                        // (typically WinAnsi) as the byte → glyph-name source
                        // and the CFF Charset as the glyph-name → GID resolver
                        // (ISO 32000-1 §9.6.6). The subsetter's own CFF Encoding
                        // table is *not* consulted directly — sparse subsetter
                        // CFF Encoding tables would silently drop most content
                        // bytes to `.notdef` otherwise.
                        //
                        // Type0 + CIDFontType0 (CFF / OpenType-CFF): Identity-H
                        // emission means the content-stream's 2-byte codes ARE
                        // the GIDs in the CFF charset; bypass harfrust Unicode
                        // shaping (which round-trips CID→Unicode→GID through
                        // the patched cmap and can drift on CFF charset
                        // positions) and feed the raw codes to
                        // render_cid_direct (G3-h). ttf-parser handles CFF
                        // outlines for sfnt-wrapped OpenType-CFF (OTTO); raw
                        // CFF streams were already wrapped by
                        // `font_dict::wrap_cff_in_opentype` at load time.
                        log::debug!(
                            "Using embedded CFF font '{}' with direct GID mapping",
                            info.base_font
                        );
                        Some((None, Arc::clone(embedded), 0, true))
                    } else {
                        log::debug!(
                            "Embedded font '{}' lacks usable cmap, falling back to system font",
                            info.base_font
                        );
                        self.load_font_data(&info.base_font)
                            .map(|(id, d, i)| (Some(id), d, i, false))
                    }
                } else {
                    self.load_font_data(&info.base_font)
                        .map(|(id, d, i)| (Some(id), d, i, false))
                }
            } else {
                self.load_font_data(pdf_font_name)
                    .map(|(id, d, i)| (Some(id), d, i, false))
            };

        if let Some((font_id, font_data, index, use_cid_to_gid)) = font_data_and_index {
            if use_cid_to_gid {
                // Direct CIDToGIDMap/CFF rendering — bypass harfrust, use ttf-parser for glyph outlines
                match self.render_cid_direct(
                    pixmap,
                    text,
                    font_info.as_deref().unwrap(),
                    &font_data,
                    index,
                    &paint,
                    base_transform,
                    gs,
                    clip_mask,
                ) {
                    Ok(advance) => return Ok(advance),
                    Err(e) => {
                        // Fall back to system font if embedded parsing fails
                        log::warn!(
                            "Direct CID/CFF rendering failed: {}, falling back to system font",
                            e
                        );
                        if let Some((fb_id, fallback_data, fallback_idx)) =
                            self.load_font_data(pdf_font_name)
                        {
                            return self.render_unicode_text(
                                pixmap,
                                &unicode_text,
                                text,
                                font_info.as_deref(),
                                Some(fb_id),
                                fallback_data,
                                fallback_idx,
                                &paint,
                                base_transform,
                                gs,
                                clip_mask,
                                pdf_font_name,
                                false,
                            );
                        }
                    }
                }
            }
            Ok(self.render_unicode_text(
                pixmap,
                &unicode_text,
                text, // raw bytes
                font_info.as_deref(),
                font_id,
                font_data,
                index,
                &paint,
                base_transform,
                gs,
                clip_mask,
                pdf_font_name,
                true, // allow_fallback
            )?)
        } else {
            let font_name = font_info
                .as_ref()
                .map(|i| i.base_font.as_str())
                .unwrap_or("unknown");
            log::warn!(
                "No font found for '{}', text may render incorrectly. \
                 Install common fonts (e.g., liberation-fonts, dejavu-fonts, or noto-fonts).",
                font_name
            );
            // Fallback to simple rendering if font not found
            Ok(self.render_text_fallback(
                pixmap,
                &unicode_text,
                &paint,
                base_transform,
                gs,
                clip_mask,
            )?)
        }
    }

    /// Decode raw PDF text bytes to a Unicode string based on font type.
    fn decode_text_to_unicode(
        &self,
        bytes: &[u8],
        font: Option<&crate::fonts::FontInfo>,
    ) -> String {
        let raw_result = if let Some(font) = font {
            let mut result = String::new();
            // Use pre-computed lookup table for performance if it's a simple font
            if font.subtype != "Type0" {
                let table = font.get_byte_to_char_table();
                for &byte in bytes {
                    let c = table[byte as usize];
                    if c != '\0' {
                        result.push(c);
                    } else {
                        // Fallback: multi-char mapping or unmapped byte
                        let char_str = font
                            .char_to_unicode(byte as u32)
                            .unwrap_or_else(|| fallback_char_to_unicode(byte as u32));
                        if char_str != "\u{FFFD}" {
                            result.push_str(&char_str);
                        }
                    }
                }
            } else {
                // Complex font: use unified iterator for robust multi-byte decoding
                for (char_code, _) in TextCharIter::new(bytes, Some(font)) {
                    let char_str = font
                        .char_to_unicode(char_code as u32)
                        .unwrap_or_else(|| fallback_char_to_unicode(char_code as u32));

                    if char_str != "\u{FFFD}" {
                        result.push_str(&char_str);
                    }
                }
            }
            result
        } else {
            // No font - fallback to Latin-1 (ISO 8859-1) encoding
            bytes.iter().map(|&b| char::from(b)).collect()
        };

        // Filter control characters from failed encoding resolution,
        // and expand presentation-form ligature code points (fi, fl, ffi,
        // ffl, st, ct, …) into their component letters so the shaper
        // passes the cluster through as ordinary glyphs instead of
        // dropping it or producing a lone box. `extract_text` already
        // does this on the extraction path via
        // `ligature_processor::get_ligature_components`; without the
        // same decomposition on the render path, words like
        // "Efficient" rasterize as "Effi  ert" because the shaper can't
        // resolve the ligature cluster against the fallback system
        // font. See issue #331 (R2).
        let mut filtered = String::with_capacity(raw_result.len());
        for c in raw_result.chars() {
            if c < '\x20' && c != '\t' && c != '\n' && c != '\r' {
                continue;
            }
            if let Some(components) = crate::text::ligature_processor::get_ligature_components(c) {
                filtered.push_str(components);
            } else {
                filtered.push(c);
            }
        }
        filtered
    }

    /// Measure-only: compute the horizontal advance of a Tj text string
    /// without painting any glyphs.
    ///
    /// Used by the operator loop when a text-showing operator falls inside an
    /// excluded OCG scope: glyphs must not be rasterised, but the text matrix
    /// still needs to advance so that any subsequent visible text in the same
    /// BT/ET block paints at the correct X position.
    ///
    /// Implements the PDF text advance formula `tx = ((w0 * Tfs) + Tc + Tw) * Th`
    /// per ISO 32000-1 §9.4.4, summing across the source-character widths exposed
    /// by [`crate::fonts::FontInfo::get_glyph_width`].
    pub fn measure_text(
        &self,
        text: &[u8],
        gs: &GraphicsState,
        font_cache: &HashMap<String, Arc<crate::fonts::FontInfo>>,
    ) -> f32 {
        let font_info = gs
            .font_name
            .as_ref()
            .and_then(|n| font_cache.get(n).cloned());
        measure_text_bytes(text, gs, font_info.as_deref())
    }

    /// Measure-only: compute the total advance of a TJ array along the
    /// active writing axis (x for WMode 0, y for WMode 1), without
    /// painting any glyphs.
    pub fn measure_tj_array(
        &self,
        array: &[TextElement],
        gs: &GraphicsState,
        font_cache: &HashMap<String, Arc<crate::fonts::FontInfo>>,
    ) -> f32 {
        let font_info = gs
            .font_name
            .as_ref()
            .and_then(|n| font_cache.get(n).cloned());
        let mut total: f32 = 0.0;
        for element in array {
            match element {
                TextElement::String(text) => {
                    total += measure_text_bytes(text, gs, font_info.as_deref());
                }
                TextElement::Offset(offset) => {
                    // PDF numeric offsets in a TJ array shift the cursor by
                    // -offset/1000 * font_size along the active writing
                    // axis. The axis swap is applied by the caller via
                    // advance_text_matrix; here we just accumulate the
                    // scalar magnitude.
                    let shift = (-offset / 1000.0) * gs.font_size;
                    total += shift;
                }
            }
        }
        total
    }

    /// Render a TJ array (text with positioning adjustments).
    ///
    /// Returns the total advance along the active writing axis (x for
    /// WMode 0, y for WMode 1) in PDF text-space units. The axis swap is
    /// applied by the caller via [`GraphicsState::advance_text_matrix`];
    /// the rasterizer never constructs a horizontal-translation matrix
    /// directly.
    ///
    /// `color_override` carries the resolution-pipeline output. It is
    /// threaded into each inner `render_text` call so the per-element
    /// paint colour is the resolved RGBA rather than the `gs.fill_*`
    /// field the operator stack carried. The existing per-call
    /// `current_gs.clone()` (needed to advance `text_matrix` between TJ
    /// elements) is the only `GraphicsState` allocation on the TJ path
    /// — the operator-arm-side clone is eliminated.
    pub fn render_tj_array(
        &self,
        pixmap: &mut Pixmap,
        array: &[TextElement],
        base_transform: Transform,
        gs: &GraphicsState,
        color_override: Option<&crate::rendering::page_renderer::ResolvedColors>,
        resources: &Object,
        doc: &PdfDocument,
        clip_mask: Option<&tiny_skia::Mask>,
        font_cache: &HashMap<String, Arc<crate::fonts::FontInfo>>,
    ) -> Result<f32> {
        let mut current_gs = gs.clone();
        let mut total_advance: f32 = 0.0;

        for element in array {
            match element {
                TextElement::String(text) => {
                    let advance = self.render_text(
                        pixmap,
                        text,
                        base_transform,
                        &current_gs,
                        color_override,
                        resources,
                        doc,
                        clip_mask,
                        font_cache,
                    )?;
                    current_gs.advance_text_matrix(advance);
                    total_advance += advance;
                }
                TextElement::Offset(offset) => {
                    let shift = (-offset / 1000.0) * current_gs.font_size;
                    current_gs.advance_text_matrix(shift);
                    total_advance += shift;
                }
            }
        }
        Ok(total_advance)
    }
}

impl Default for TextRasterizer {
    fn default() -> Self {
        Self::new()
    }
}
