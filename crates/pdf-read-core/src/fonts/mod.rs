//! Font handling and encoding.
//!
//! This module provides font dictionary parsing, encoding handling,
//! and ToUnicode CMap parsing for accurate text extraction.
//!
mod adobe_glyph_list;
/// CFF font encoding parser for extracting built-in encoding from CFF FontFile data.
pub mod cff_encoding;
pub mod character_mapper;
/// CID to Unicode mappings for predefined Adobe CJK character collections.
pub mod cid_mappings;
pub mod cmap;
pub mod encoding_normalizer;
pub mod font_dict; // Private module - only used internally by font_dict
/// Process-level cross-document font cache for batch processing.
pub mod global_cache;
pub mod non_text_detection;
/// Adobe predefined CIDFont base-name registry for substitution when the PDF
/// references one of the Adobe-Japan1 / Adobe-GB1 / Adobe-CNS1 / Adobe-Korea1
/// faces (Ryumin-Light, GothicBBB-Medium, STSong-Light, …) without embedding
/// glyph outlines. ISO 32000-2 §9.7.5.2 mandates support for these collections.
pub mod predefined_cidfont;
pub mod provenance;
/// TrueType font CMap parsing for glyph-to-character mapping.
pub mod truetype_cmap;
/// Type 1 font encoding parser for extracting built-in encoding from FontFile data.
pub mod type1_encoding;

pub use character_mapper::CharacterMapper;
pub use cmap::{parse_tounicode_cmap, CMap, LazyCMap};
pub use encoding_normalizer::EncodingNormalizer;
pub use font_dict::{CIDSystemInfo, CIDToGIDMap, Encoding, FontInfo, VerticalMetrics};
pub use non_text_detection::{
    CharacterConfidence, ConfidenceReason, NonTextDetector, NonTextStats,
};
pub use provenance::MappingProvenance;
pub use truetype_cmap::TrueTypeCMap;
