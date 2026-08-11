//! CFF (Compact Font Format) encoding parser.
//!
//! Parses the built-in encoding table from CFF font programs and resolves
//! PDF content-stream bytes to glyph IDs.
//!
//! # Two byte → GID resolution paths
//!
//! Simple CFF fonts have two potential sources of byte → glyph mapping
//! information, and PDFs decide between them according to ISO 32000-1
//! §9.6.6:
//!
//! 1. **The PDF font dictionary's `/Encoding`** is authoritative for
//!    simple fonts (Type 1 / TrueType / CFF). It supplies byte → glyph
//!    *name*; the CFF Charset then resolves the name → SID → GID.
//!    [`parse_cff_gid_mapping_with_pdf_encoding`] implements this path.
//!
//! 2. **The CFF font program's own Encoding table** (CFF Tech Note #5176
//!    §12) supplies byte → GID directly. This is the *fallback* when no
//!    PDF-level encoding is supplied (e.g. an `Encoding::Identity` caller).
//!    [`parse_cff_gid_mapping`] implements this path.
//!
//! Subsetter-emitted CFF Encoding tables are frequently sparse —
//! some prepress subsetters commonly emit only `0x20 → space` and
//! `0x41 → A` while the Charset enumerates the full subset — so callers
//! that have the PDF `/Encoding` in hand should always go through the
//! `_with_pdf_encoding` entrypoint. The CFF Encoding path is preserved
//! for backwards compatibility and as a final fallback when the PDF side
//! cannot supply a byte → name resolver.
//!
//! Per PDF spec §9.6.6.2, when no `/BaseEncoding` is specified in an
//! encoding dictionary, the implicit base encoding is the font program's
//! built-in encoding — this module also provides [`parse_cff_encoding`]
//! for that legacy lookup.

use std::collections::HashMap;

mod names;
mod parser;

use names::{
    glyph_name_to_sid, mac_roman_byte_to_name, sid_to_name, standard_encoding_byte_to_name,
};
pub use parser::{
    parse_cff_encoding, parse_cff_gid_mapping, parse_cff_gid_mapping_with_pdf_encoding,
};

#[cfg(test)]
mod tests;
