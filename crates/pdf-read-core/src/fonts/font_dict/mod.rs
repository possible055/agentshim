//! Font dictionary parsing.
//!
//! This module handles parsing of PDF font dictionaries and encoding information.
//! Fonts in PDF can have various encodings, and the ToUnicode CMap provides the
//! most accurate character-to-Unicode mapping.

use super::adobe_glyph_list::ADOBE_GLYPH_LIST;
use crate::document::PdfDocument;
use crate::error::{Error, Result};
use crate::fonts::cmap::LazyCMap;
use crate::fonts::TrueTypeCMap;
use crate::layout::text_block::FontWeight;
use crate::object::Object;
use std::collections::HashMap;
use std::sync::Arc;

/// Name-derived Standard-14 classification of a font, resolved once and
/// memoized (see [`FontInfo::std14_memo`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Std14Flags {
    /// Font is one of the Times family.
    pub is_times: bool,
    /// Font is one of the Courier (monospace) family.
    pub is_courier: bool,
    /// Font name carries a Bold marker.
    pub is_bold: bool,
    /// Font name carries a BoldItalic marker.
    pub is_bold_italic: bool,
    /// Font is one of the Helvetica family.
    pub is_helvetica: bool,
    /// Font name carries an Italic marker.
    pub is_italic: bool,
}

/// Font information extracted from a PDF font dictionary.
#[derive(Debug, Clone)]
pub struct FontInfo {
    /// Base font name (e.g., "Times-Roman", "Helvetica-Bold")
    pub base_font: String,
    /// Font subtype (e.g., "Type1", "TrueType", "Type0")
    pub subtype: String,
    /// Encoding information
    pub encoding: Encoding,
    /// ToUnicode CMap (character code to Unicode mapping)
    /// Lazily parsed on first character lookup for improved performance
    pub to_unicode: Option<LazyCMap>,
    /// Font weight from FontDescriptor (400 = normal, 700 = bold)
    pub font_weight: Option<i32>,
    /// Font descriptor flags (bit field)
    /// Bit 1: FixedPitch, Bit 2: Serif, Bit 3: Symbolic, Bit 4: Script,
    /// Bit 6: Nonsymbolic, Bit 7: Italic
    /// PDF Spec: ISO 32000-1:2008, Table 5.20
    pub flags: Option<i32>,
    /// Stem thickness (vertical) from FontDescriptor (used for weight inference)
    /// PDF Spec: ISO 32000-1:2008, Section 9.6.2
    /// Typical values: <80 = light, 80-110 = normal/medium, >110 = bold
    pub stem_v: Option<f32>,
    /// Ascent above the baseline (fraction of em, from FontDescriptor /Ascent).
    /// Converted from PDF's 1/1000-em units to a fraction of em (raw value ÷ 1000).
    /// Defaults to 0.95 when the font descriptor is absent (matching Poppler's fallback).
    pub ascent: f32,
    /// Descent below the baseline (fraction of em, from FontDescriptor /Descent).
    /// Converted from PDF's 1/1000-em units to a fraction of em; always ≤ 0.
    /// Defaults to -0.35 when the font descriptor is absent (matching Poppler's fallback).
    pub descent: f32,
    /// Embedded TrueType font data (from FontFile2 stream)
    /// Shared via Arc to avoid expensive cloning
    pub embedded_font_data: Option<Arc<Vec<u8>>>,
    /// Lazily-extracted TrueType cmap table (GID to Unicode mappings).
    /// Used as fallback when ToUnicode CMap is missing.
    /// Initialized on first access via `truetype_cmap()` accessor to avoid
    /// the 10-25ms per-font extraction cost when ToUnicode resolves all chars.
    pub truetype_cmap: std::sync::OnceLock<Option<TrueTypeCMap>>,
    /// Lazily-extracted embedded TrueType/CFF `post`-table glyph names,
    /// indexed by GID. `None` element = no name for that GID (post format 3,
    /// or the glyph name table is absent). Used by §9.10.2 Priority 3c
    /// fallback in `decode_char_to_unicode`: when `truetype_cmap.get_unicode`
    /// misses, we try this glyph name via `glyph_name_to_unicode` (AGL +
    /// `uniXXXX`/`uXXXXX` synth) before falling through to the hardcoded
    /// `gid_to_standard_glyph_name` ASCII map and CID-as-Unicode last
    /// resort. Resolves `•` → `❍` substitution and `fi`/`fl` ligature
    /// corruption on Identity-H subset fonts without `CIDToGIDMap`.
    ///
    pub embedded_glyph_names: std::sync::OnceLock<Option<Vec<Option<String>>>>,
    /// Whether this font has an embedded TrueType font (FontFile2).
    /// Controls whether lazy truetype_cmap extraction is attempted.
    pub is_truetype_font: bool,
    /// CID to GID mapping (Type0 fonts only, Phase 3)
    /// Converts Character IDs in the PDF to Glyph IDs in the embedded font
    /// Used to look up Unicode values via the TrueType cmap table
    /// Phase 3: Enables CFF/OpenType support via CIDToGIDMap parsing
    pub cid_to_gid_map: Option<CIDToGIDMap>,
    /// CIDFont character collection info (Type0 fonts only)
    /// Identifies the character set (e.g., Adobe-Japan1, Adobe-GB1)
    pub cid_system_info: Option<CIDSystemInfo>,
    /// CIDFont subtype ("CIDFontType0" for CFF, "CIDFontType2" for TrueType)
    pub cid_font_type: Option<String>,
    /// `FontMatrix[a]` element — scales glyph-space widths to text-space units.
    /// Standard Type1/TrueType: 0.001 (widths in 1/1000 em).
    /// Type3 with `FontMatrix [1 0 0 1 0 0]`: 1.0 (widths already in text-space units).
    /// `advance_in_text_space = width × font_matrix_a × font_size`
    pub font_matrix_a: f32,
    /// Character widths in 1000ths of em (PDF units)
    /// For simple fonts (Type1, TrueType): array indexed by (char_code - first_char)
    /// PDF Spec: ISO 32000-1:2008, Section 9.7.4
    pub widths: Option<Vec<f32>>,
    /// First character code covered by widths array
    /// Used to map character codes to width array indices
    pub first_char: Option<u32>,
    /// Last character code covered by widths array
    pub last_char: Option<u32>,
    /// Default width for characters not in widths array (in 1000ths of em)
    /// Typical values: 500-600 for proportional fonts, 600 for monospace
    pub default_width: f32,
    /// CID to width mapping for Type0 (CIDFont) fonts
    /// Per PDF Spec ISO 32000-1:2008, Section 9.7.4.3
    /// Widths in 1000ths of em. Uses HashMap for sparse CID distributions.
    pub cid_widths: Option<HashMap<u16, f32>>,
    /// Default width for CIDs not in cid_widths (Type0 fonts only)
    /// Per PDF Spec: default is 1000 if /DW not specified
    pub cid_default_width: f32,
    /// Whether /DW was explicitly present in the CIDFont dictionary.
    /// Used by has_explicit_widths() and get_glyph_width() to distinguish
    /// a spec-default 1000 from an authored 1000 (F14/F15 fix).
    pub has_explicit_dw: bool,
    /// Multi-character encoding map for compound glyph names (e.g. f_f → "ff")
    /// Stores mappings from character code to multi-char strings
    pub multi_char_map: HashMap<u8, String>,
    /// CFF byte_code → glyph_id mapping for embedded CFF subset fonts.
    /// Allows direct glyph rendering without Unicode cmap.
    pub cff_gid_map: Option<HashMap<u8, u16>>,
    /// Pre-computed byte→char lookup for simple (non-Type0) fonts.
    /// Index by byte value (0-255). '\0' means "use full char_to_unicode fallback".
    /// Built lazily on first text decode. Avoids per-byte HashMap lookups.
    pub byte_to_char_table: std::sync::OnceLock<[char; 256]>,
    /// Per-font memo of `char_to_unicode`. Type0/CID fonts have no
    /// `byte_to_char_table`, so without this each glyph re-runs the decode
    /// cascade. `Arc<Mutex<…>>` keeps `FontInfo: Clone` (clones share the memo).
    pub type0_unicode_memo:
        std::sync::Arc<std::sync::Mutex<std::collections::HashMap<u32, Option<String>>>>,
    /// Pre-computed byte→width lookup for simple (non-Type0) fonts.
    /// Index by byte value (0-255). Built lazily on first advance_position call.
    /// Eliminates per-byte bounds check and subtraction in get_glyph_width.
    pub byte_to_width_table: std::sync::OnceLock<[f32; 256]>,
    /// Memo of [`FontInfo::get_font_weight`]. The name-based fallback lowercases
    /// `base_font` and runs a dozen substring searches; text extraction asks for
    /// the weight once per glyph, where the answer is loop-invariant.
    pub weight_memo: std::sync::OnceLock<FontWeight>,
    /// Memo of [`FontInfo::is_italic`] — same per-glyph hot path as `weight_memo`.
    pub italic_memo: std::sync::OnceLock<bool>,
    /// Memo of the Standard-14 name classification. `get_standard_font_width`
    /// is called per glyph and otherwise re-strips the subset prefix and
    /// re-scans a 15-name table every time.
    pub std14_memo: std::sync::OnceLock<Option<Std14Flags>>,
    /// Raw `/Differences` glyph names retained by character code (simple fonts).
    /// Populated alongside the `Encoding::Custom` map during `parse_encoding`,
    /// but unlike the Custom map (which stores the *resolved* char) this keeps the
    /// authoritative glyph *name* the writer assigned via the encoding dictionary's
    /// `/Differences` array (ISO 32000-1 §9.6.6.1, Table 114). Used by
    /// `glyph_name_for_code` to recover punctuation (`period`/`comma`/`hyphen`/
    /// `minus`) when an upstream decode yields a non-sensible symbol — see the
    /// glyph-name-gated interceptions in `char_to_unicode`.
    pub diff_glyph_names: HashMap<u8, String>,
    /// Writing mode resolved from this font's encoding and (when available)
    /// from the embedded CMap stream's `/WMode` directive.
    ///
    /// - `0` (default): horizontal writing — glyph advance along x-axis.
    /// - `1`: vertical writing (tategaki) — glyph advance along y-axis with
    ///   per-CID vertical-origin offset applied per glyph.
    ///
    /// Resolution rules (highest precedence first):
    /// 1. The embedded CMap stream's `/WMode` directive when one is parsed
    ///    (via `LazyCMap::wmode()` on the encoding's CMap).
    /// 2. Predefined PDF CMap name ending in `-V` (Identity-V, UniJIS-UTF16-V,
    ///    UniGB-UTF16-V, UniCNS-UTF16-V, UniKS-UTF16-V) or the bare legacy
    ///    `V`. The original encoding name is retained even when the
    ///    `Encoding` enum collapses `Identity-H`/`Identity-V` into
    ///    `Encoding::Identity`.
    /// 3. Otherwise `0`.
    pub wmode: u8,
    /// Per-CID vertical-writing metrics parsed from the CIDFont's `/W2`
    /// array (ISO 32000-1 §9.7.4.3). `None` for horizontal-only fonts so
    /// they pay no allocation/hash-lookup cost.
    pub cid_vertical_metrics: Option<HashMap<u16, VerticalMetrics>>,
    /// Default vertical metrics for CIDs not covered by `cid_vertical_metrics`.
    /// Parsed from `/DW2` (defaults to [`VerticalMetrics::SPEC_DEFAULT`] when
    /// `/DW2` is absent). Held by value because the struct is `Copy`.
    pub cid_default_vertical_metrics: VerticalMetrics,
    /// `Some(collection)` when this is a Type0 CIDFont referencing one of
    /// Adobe's predefined CJK base names (`Ryumin-Light`, `GothicBBB-Medium`,
    /// `STSong-Light`, `MHei-Medium`, `HYSMyeongJo-Medium`, …), has no
    /// embedded font program (no `/FontFile{,2,3}` key on either the Type0
    /// wrapper's or the CIDFont descendant's descriptor), AND uses an
    /// Identity charcode→CID `/Encoding` (Identity-H/V or an
    /// Adobe-collection identity CMap stream). ISO 32000-2 §9.7.5.2 requires
    /// a conforming reader to supply glyphs for these character collections;
    /// the renderer consults this field to route the paint through a bundled
    /// covering font (see [`super::predefined_cidfont`]) and convert each CID
    /// through the appropriate [`super::cid_mappings`] table to a Unicode
    /// code point. The collection follows the descendant's `/CIDSystemInfo`
    /// Ordering when it names a known collection, falling back to the
    /// name-derived collection for Identity/unknown orderings.
    ///
    /// `None` for every other font, including:
    /// - Type0 fonts whose CIDFont declares an embedded program — even when
    ///   that program fails to load/decode, substitution would mask the
    ///   decode defect; the failure is logged instead;
    /// - Type0 fonts with a non-Identity predefined CMap (`90ms-RKSJ-H`,
    ///   `GBK-EUC-H`, …) whose charcodes are raw legacy multi-byte values,
    ///   not CIDs — unsupported until a charcode→CID CMap pass is wired;
    /// - Type0 fonts whose base name is not in the predefined registry (we
    ///   cannot safely guess a substitution);
    /// - Simple Type1 / TrueType fonts.
    pub cjk_substitution: Option<super::predefined_cidfont::CharacterCollection>,
}

/// Font encoding types.
#[derive(Debug, Clone)]
pub enum Encoding {
    /// Standard PDF encoding (WinAnsiEncoding, MacRomanEncoding, etc.)
    Standard(String),
    /// Custom encoding with explicit character mappings
    Custom(HashMap<u8, char>),
    /// Identity encoding (typically used for CID fonts)
    Identity,
}

/// CID to GID mapping for Type 2 CIDFonts (TrueType-based)
/// Per PDF Spec ISO 32000-1:2008, Section 9.7.4.2
///
/// This mapping converts Character IDs (CIDs) in the PDF document to Glyph IDs (GIDs)
/// in the embedded TrueType font, which can then be mapped to Unicode via the cmap table.
#[derive(Debug, Clone)]
pub enum CIDToGIDMap {
    /// Identity mapping: CID == GID (default, most common)
    /// Used when each character ID directly corresponds to a glyph ID
    Identity,

    /// Explicit mapping: CID → GID via uint16 stream
    /// Stream format: GID at bytes [2*CID, 2*CID+1], big-endian
    /// Used for non-standard glyph ID assignments
    Explicit(Vec<u16>),
}

impl CIDToGIDMap {
    /// Convert a Character ID (CID) to a Glyph ID (GID) using this mapping.
    ///
    /// Per PDF Spec ISO 32000-1:2008, Section 9.7.4.2:
    /// - Identity mapping: CID == GID (most common, default)
    /// - Explicit mapping: Use uint16 array lookup
    ///
    /// # Arguments
    ///
    /// * `cid` - The Character ID from the PDF document
    ///
    /// # Returns
    ///
    /// The corresponding Glyph ID in the embedded font
    pub fn get_gid(&self, cid: u16) -> u16 {
        match self {
            CIDToGIDMap::Identity => cid,
            CIDToGIDMap::Explicit(gid_array) => {
                if (cid as usize) < gid_array.len() {
                    gid_array[cid as usize]
                } else {
                    // Out of range - fall back to identity mapping
                    cid
                }
            }
        }
    }
}

/// CIDFont character collection identifier
/// Per PDF Spec ISO 32000-1:2008, Section 9.7.4.2
///
/// Identifies which character encoding the CIDFont uses, such as:
/// - Adobe-Japan1: Japanese text
/// - Adobe-GB1: Simplified Chinese
/// - Adobe-CNS1: Traditional Chinese
/// - Adobe-Korea1: Korean
#[derive(Debug, Clone)]
pub struct CIDSystemInfo {
    /// Registry name (typically "Adobe")
    pub registry: String,

    /// Ordering string (e.g., "Japan1", "GB1", "CNS1", "Korea1")
    pub ordering: String,

    /// Supplement number (version of the character collection)
    pub supplement: i32,
}

/// Per-CID vertical-writing metrics from a CIDFont's `/W2` array.
///
/// Per ISO 32000-1:2008 §9.7.4.3 and the Adobe CMap & CIDFont Files
/// Specification §9.7. In vertical writing mode the glyph advances along the
/// y-axis (not the x-axis) and is shifted from its default horizontal origin
/// to a vertical origin so that the glyph stacks correctly within a column.
///
/// All values are in 1000ths-of-em (glyph-space units), matching the
/// convention used throughout PDF font dictionaries.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VerticalMetrics {
    /// `w1y`: vertical displacement (advance) of the glyph along the y-axis.
    ///
    /// Typically negative (around `-1000` for a full-em CJK glyph) because PDF
    /// user space has y increasing upward, while vertical text advances
    /// downward. The text matrix is translated by `w1y * font_size / 1000`
    /// after the glyph is painted.
    pub w1y: f32,

    /// `v_x`: x-component of the vector from the default (horizontal) origin
    /// to the vertical origin, in 1000ths-of-em.
    ///
    /// Spec default `500` (half-em) places the vertical origin at the glyph's
    /// horizontal center, which is correct for monospaced full-width CJK
    /// glyphs.
    pub v_x: f32,

    /// `v_y`: y-component of the vertical-origin offset, in 1000ths-of-em.
    ///
    /// Spec default `880` places the vertical origin near the top of the em.
    pub v_y: f32,
}

impl VerticalMetrics {
    /// Spec default per ISO 32000-1 §9.7.4.3: vertical origin at
    /// `(500, 880)` and glyph displacement `-1000` (one full em downward).
    pub const SPEC_DEFAULT: VerticalMetrics = VerticalMetrics {
        w1y: -1000.0,
        v_x: 500.0,
        v_y: 880.0,
    };
}

/// Decide writing mode from a predefined PDF CMap name.
///
/// Per ISO 32000-1 §9.7.5.2 (Table 118) and the Adobe CMap & CIDFont Files
/// Specification, predefined CMap names whose suffix is `-V` (e.g.
/// `Identity-V`, `UniJIS-UTF16-V`, `UniGB-UTF16-V`, `UniCNS-UTF16-V`,
/// `UniKS-UTF16-V`, `GBK-EUC-V`, `90ms-RKSJ-V`, …) and the bare legacy `V`
/// declare vertical writing (`/WMode 1`). Every other name implies
/// horizontal writing (`/WMode 0`).
///
/// This function is the canonical name-to-wmode decision used by both
/// `FontInfo::resolve_encoding_writing_mode` and the encoding-name fallback
/// inside `FontInfo::from_dict`.
pub(crate) fn wmode_from_predefined_cmap_name(name: &str) -> u8 {
    if name == "V" || name.ends_with("-V") {
        1
    } else {
        0
    }
}

mod accessors;
mod cid_metrics;
mod cjk;
mod classification;
mod descendants;
mod descriptors;
mod encoding;
mod glyph_methods;
mod glyph_names;
mod loading;
mod standard_encodings;
mod symbolic_encodings;
mod unicode;
mod widths;

pub(crate) use glyph_names::{glyph_name_to_unicode, glyph_name_to_unicode_string};
pub use symbolic_encodings::pdfdoc_encoding_lookup;

use cjk::*;
#[cfg(test)]
use descriptors::wrap_cff_in_opentype;
use descriptors::{parse_font_descriptor, DescriptorData};
use glyph_names::{
    expand_ligature_char, is_ligature_char, is_non_sensible_symbol, normalize_cjk_radical_forms,
    punctuation_unicode_for_glyph_name, shift_jis_to_unicode,
};
use standard_encodings::{builtin_encoding_looks_like_cipher, standard_encoding_lookup};
use symbolic_encodings::{symbol_encoding_lookup, zapf_dingbats_encoding_lookup};

#[cfg(test)]
mod tests;
