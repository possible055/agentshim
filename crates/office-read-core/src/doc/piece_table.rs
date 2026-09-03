//! Piece table parsing for Word binary documents.
//!
//! The piece table maps character positions to byte ranges in the WordDocument stream.
//! Each piece can be either:
//! - Compressed (CP1252): 1 byte per character, fc has bit 30 set, actual offset = (fc & ~0x40000000) / 2
//! - Unicode (UTF-16LE): 2 bytes per character, fc is used directly

use super::error::{DocError, Result};

/// A single piece descriptor.
#[derive(Debug, Clone)]
pub struct Piece {
    /// Character position range start (inclusive).
    pub cp_start: u32,
    /// Character position range end (exclusive).
    pub cp_end: u32,
    /// File offset in the WordDocument stream.
    pub fc: u32,
    /// Whether this piece uses compressed (CP1252) encoding.
    pub is_compressed: bool,
}

/// Parse the CLX structure to extract the piece table.
///
/// The CLX contains:
/// - Optional Grpprl entries (type 0x01): skip them.
/// - Pcdt entry (type 0x02): the piece table.
pub fn parse_clx(data: &[u8]) -> Result<Vec<Piece>> {
    let mut pos = 0;

    // Skip Grpprl entries.
    while pos < data.len() && data[pos] == 0x01 {
        if pos + 3 > data.len() {
            return Err(DocError::InvalidPieceTable("Grpprl truncated".into()));
        }
        let size = u16::from_le_bytes([data[pos + 1], data[pos + 2]]) as usize;
        pos = pos
            .checked_add(3)
            .and_then(|offset| offset.checked_add(size))
            .filter(|end| *end <= data.len())
            .ok_or_else(|| DocError::InvalidPieceTable("Grpprl is truncated".into()))?;
    }

    // Now we should be at the Pcdt (type 0x02).
    if pos >= data.len() || data[pos] != 0x02 {
        return Err(DocError::InvalidPieceTable(format!(
            "expected Pcdt (0x02) at offset {pos}, found {:?}",
            data.get(pos)
        )));
    }
    pos += 1;

    if pos + 4 > data.len() {
        return Err(DocError::InvalidPieceTable("Pcdt size truncated".into()));
    }
    let pcdt_size =
        u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
    pos += 4;

    let pcd_end = pos
        .checked_add(pcdt_size)
        .filter(|end| *end <= data.len())
        .ok_or_else(|| DocError::InvalidPieceTable("Pcdt is truncated".into()))?;
    if pcd_end != data.len() {
        return Err(DocError::InvalidPieceTable(
            "unexpected bytes after Pcdt".into(),
        ));
    }
    let pcd_data = &data[pos..pcd_end];
    parse_plc_pcd(pcd_data)
}

/// Parse the PlcPcd structure (array of CPs + array of PCDs).
///
/// Structure:
/// - (n+1) u32 character positions (CPs)
/// - n PCD entries (8 bytes each)
///
/// Where n = (size - 4) / 12 (solve for: (n+1)*4 + n*8 = size)
fn parse_plc_pcd(data: &[u8]) -> Result<Vec<Piece>> {
    if data.len() < 8 {
        return Err(DocError::InvalidPieceTable("PlcPcd too small".into()));
    }

    // n pieces: (n+1)*4 + n*8 = data.len() → n = (data.len() - 4) / 12
    if !(data.len() - 4).is_multiple_of(12) {
        return Err(DocError::InvalidPieceTable(
            "PlcPcd length is misaligned".into(),
        ));
    }
    let n = (data.len() - 4) / 12;
    if n == 0 {
        return Ok(Vec::new());
    }

    let cp_array_size = (n + 1) * 4;
    if cp_array_size + n * 8 > data.len() {
        return Err(DocError::InvalidPieceTable("PlcPcd size mismatch".into()));
    }
    crate::budget::check_model_items("office_doc_pieces", n)
        .map_err(crate::map_office_error_to_cfb)
        .map_err(DocError::from)?;
    crate::budget::charge_model_items("office_doc_pieces", n)
        .map_err(crate::map_office_error_to_cfb)
        .map_err(DocError::from)?;

    let mut pieces = Vec::with_capacity(n);

    for i in 0..n {
        let cp_start = u32::from_le_bytes([
            data[i * 4],
            data[i * 4 + 1],
            data[i * 4 + 2],
            data[i * 4 + 3],
        ]);
        let cp_end = u32::from_le_bytes([
            data[(i + 1) * 4],
            data[(i + 1) * 4 + 1],
            data[(i + 1) * 4 + 2],
            data[(i + 1) * 4 + 3],
        ]);

        // PCD at offset cp_array_size + i * 8.
        let pcd_offset = cp_array_size + i * 8;
        // PCD structure: [u16 unused][u32 fc][u16 prm]
        let fc = u32::from_le_bytes([
            data[pcd_offset + 2],
            data[pcd_offset + 3],
            data[pcd_offset + 4],
            data[pcd_offset + 5],
        ]);

        // Bit 30 of fc indicates compressed encoding.
        let is_compressed = (fc & 0x40000000) != 0;

        if cp_end <= cp_start || (i == 0 && cp_start != 0) {
            return Err(DocError::InvalidPieceTable(
                "piece character ranges are not strictly increasing".into(),
            ));
        }

        pieces.push(Piece {
            cp_start,
            cp_end,
            fc,
            is_compressed,
        });
    }

    Ok(pieces)
}

/// Extract text from the WordDocument stream using the piece table.
pub fn extract_text(word_doc: &[u8], pieces: &[Piece], max_chars: u32) -> Result<String> {
    let mut text = String::new();
    let mut observed_chars = 0_usize;

    for piece in pieces {
        crate::budget::check_cancelled()
            .map_err(crate::map_office_error_to_cfb)
            .map_err(DocError::from)?;
        if piece.cp_start >= max_chars {
            break;
        }

        let char_count = piece.cp_end.min(max_chars) - piece.cp_start;
        observed_chars = observed_chars.saturating_add(char_count as usize);
        crate::budget::check_model_items("office_doc_characters", observed_chars)
            .map_err(crate::map_office_error_to_cfb)
            .map_err(DocError::from)?;
        crate::budget::check_model_text_bytes(
            "office_doc_text_bytes",
            observed_chars.saturating_mul(4),
        )
        .map_err(crate::map_office_error_to_cfb)
        .map_err(DocError::from)?;

        if piece.is_compressed {
            // Compressed: 1 byte per character, CP1252.
            // Actual byte offset = (fc & ~0x40000000) / 2
            let byte_offset = ((piece.fc & !0x40000000) / 2) as usize;
            let byte_count = char_count as usize;

            let end = byte_offset
                .checked_add(byte_count)
                .filter(|end| *end <= word_doc.len())
                .ok_or_else(|| {
                    DocError::InvalidPieceTable("compressed piece is out of range".into())
                })?;
            for &b in &word_doc[byte_offset..end] {
                text.push(cp1252_to_char(b));
            }
        } else {
            // Unicode: 2 bytes per character, UTF-16LE.
            let byte_offset = piece.fc as usize;
            let byte_count = (char_count as usize)
                .checked_mul(2)
                .ok_or_else(|| DocError::InvalidPieceTable("piece size overflow".into()))?;
            let end = byte_offset
                .checked_add(byte_count)
                .filter(|end| *end <= word_doc.len())
                .ok_or_else(|| {
                    DocError::InvalidPieceTable("Unicode piece is out of range".into())
                })?;
            let code_units = word_doc[byte_offset..end]
                .chunks_exact(2)
                .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]));
            for character in std::char::decode_utf16(code_units) {
                text.push(character.unwrap_or(char::REPLACEMENT_CHARACTER));
            }
        }
    }

    if pieces.last().is_none_or(|piece| piece.cp_end < max_chars) {
        return Err(DocError::InvalidPieceTable(
            "piece table does not cover main document text".into(),
        ));
    }
    crate::budget::charge_model_text("office_doc_text_bytes", text.len())
        .map_err(crate::map_office_error_to_cfb)
        .map_err(DocError::from)?;

    Ok(text)
}

/// Convert a CP1252 byte to a Unicode char.
fn cp1252_to_char(b: u8) -> char {
    // CP1252 is identical to Latin-1 except for bytes 0x80-0x9F.
    match b {
        0x80 => '\u{20AC}', // €
        0x82 => '\u{201A}', // ‚
        0x83 => '\u{0192}', // ƒ
        0x84 => '\u{201E}', // „
        0x85 => '\u{2026}', // …
        0x86 => '\u{2020}', // †
        0x87 => '\u{2021}', // ‡
        0x88 => '\u{02C6}', // ˆ
        0x89 => '\u{2030}', // ‰
        0x8A => '\u{0160}', // Š
        0x8B => '\u{2039}', // ‹
        0x8C => '\u{0152}', // Œ
        0x8E => '\u{017D}', // Ž
        0x91 => '\u{2018}', // '
        0x92 => '\u{2019}', // '
        0x93 => '\u{201C}', // "
        0x94 => '\u{201D}', // "
        0x95 => '\u{2022}', // •
        0x96 => '\u{2013}', // –
        0x97 => '\u{2014}', // —
        0x98 => '\u{02DC}', // ˜
        0x99 => '\u{2122}', // ™
        0x9A => '\u{0161}', // š
        0x9B => '\u{203A}', // ›
        0x9C => '\u{0153}', // œ
        0x9E => '\u{017E}', // ž
        0x9F => '\u{0178}', // Ÿ
        _ => b as char,
    }
}

/// Convert special Word characters to readable text.
pub fn sanitize_text(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\r' => result.push('\n'),                       // Paragraph mark
            '\x07' => result.push('\t'),                     // Cell/row mark → tab
            '\x0C' => result.push('\n'),                     // Page break / section break
            '\x0B' => result.push('\n'),                     // Vertical tab → newline
            '\x01' | '\x08' | '\x13' | '\x14' | '\x15' => {} // Field codes, picture, etc. — skip
            _ => result.push(ch),
        }
    }
    result
}

#[cfg(any())]
mod tests {
    use super::*;

    #[test]
    fn parse_clx_with_one_piece() {
        let mut clx = Vec::new();
        // Pcdt marker.
        clx.push(0x02);
        // Size of PlcPcd: (1+1)*4 + 1*8 = 16
        clx.extend_from_slice(&16u32.to_le_bytes());
        // CP[0] = 0
        clx.extend_from_slice(&0u32.to_le_bytes());
        // CP[1] = 10
        clx.extend_from_slice(&10u32.to_le_bytes());
        // PCD: [u16 unused=0][u32 fc=0x40000100 (compressed, offset=0x80)][u16 prm=0]
        clx.extend_from_slice(&0u16.to_le_bytes());
        clx.extend_from_slice(&0x40000100u32.to_le_bytes());
        clx.extend_from_slice(&0u16.to_le_bytes());

        let pieces = parse_clx(&clx).unwrap();
        assert_eq!(pieces.len(), 1);
        assert_eq!(pieces[0].cp_start, 0);
        assert_eq!(pieces[0].cp_end, 10);
        assert!(pieces[0].is_compressed);
    }

    #[test]
    fn parse_clx_with_grpprl_prefix() {
        let mut clx = Vec::new();
        // Grpprl: type=0x01, size=3, data=[0,0,0]
        clx.push(0x01);
        clx.extend_from_slice(&3u16.to_le_bytes());
        clx.extend_from_slice(&[0, 0, 0]);
        // Pcdt
        clx.push(0x02);
        clx.extend_from_slice(&16u32.to_le_bytes());
        clx.extend_from_slice(&0u32.to_le_bytes());
        clx.extend_from_slice(&5u32.to_le_bytes());
        clx.extend_from_slice(&0u16.to_le_bytes());
        clx.extend_from_slice(&0x40000000u32.to_le_bytes());
        clx.extend_from_slice(&0u16.to_le_bytes());

        let pieces = parse_clx(&clx).unwrap();
        assert_eq!(pieces.len(), 1);
        assert_eq!(pieces[0].cp_end, 5);
    }

    #[test]
    fn extract_compressed_text() {
        // Build a word_doc with "Hello" at byte offset 0x80 (fc=0x40000100, offset = 0x100/2 = 0x80)
        let mut word_doc = vec![0u8; 256];
        let text_offset = 0x80;
        word_doc[text_offset..text_offset + 5].copy_from_slice(b"Hello");

        let pieces = vec![Piece {
            cp_start: 0,
            cp_end: 5,
            fc: 0x40000100, // compressed, offset = 0x100/2 = 0x80
            is_compressed: true,
        }];

        let text = extract_text(&word_doc, &pieces, 5);
        assert_eq!(text, "Hello");
    }

    #[test]
    fn extract_unicode_text() {
        let mut word_doc = vec![0u8; 256];
        let fc = 100u32;
        // "Hi" in UTF-16LE at offset 100
        word_doc[100] = b'H';
        word_doc[101] = 0;
        word_doc[102] = b'i';
        word_doc[103] = 0;

        let pieces = vec![Piece {
            cp_start: 0,
            cp_end: 2,
            fc,
            is_compressed: false,
        }];

        let text = extract_text(&word_doc, &pieces, 2);
        assert_eq!(text, "Hi");
    }

    #[test]
    fn extract_multiple_pieces() {
        let mut word_doc = vec![0u8; 512];
        // Piece 1: compressed "AB" at offset 0x80
        word_doc[0x80] = b'A';
        word_doc[0x81] = b'B';
        // Piece 2: compressed "CD" at offset 0x90
        word_doc[0x90] = b'C';
        word_doc[0x91] = b'D';

        let pieces = vec![
            Piece {
                cp_start: 0,
                cp_end: 2,
                fc: 0x40000100, // offset = 0x80
                is_compressed: true,
            },
            Piece {
                cp_start: 2,
                cp_end: 4,
                fc: 0x40000120, // offset = 0x90
                is_compressed: true,
            },
        ];

        let text = extract_text(&word_doc, &pieces, 4);
        assert_eq!(text, "ABCD");
    }

    #[test]
    fn sanitize_paragraph_marks() {
        assert_eq!(sanitize_text("Hello\rWorld"), "Hello\nWorld");
        assert_eq!(sanitize_text("A\x0CB"), "A\nB");
        assert_eq!(sanitize_text("A\x07B"), "A\tB");
    }

    #[test]
    fn sanitize_field_codes_stripped() {
        assert_eq!(
            sanitize_text("before\x13FIELD\x14result\x15after"),
            "beforeFIELDresultafter"
        );
    }

    #[test]
    fn cp1252_special_chars() {
        assert_eq!(cp1252_to_char(0x80), '€');
        assert_eq!(cp1252_to_char(0x93), '\u{201C}');
        assert_eq!(cp1252_to_char(0x94), '\u{201D}');
        assert_eq!(cp1252_to_char(0x41), 'A');
    }

    #[test]
    fn max_chars_limits_output() {
        let mut word_doc = vec![0u8; 256];
        word_doc[0x80..0x85].copy_from_slice(b"Hello");

        let pieces = vec![Piece {
            cp_start: 0,
            cp_end: 5,
            fc: 0x40000100,
            is_compressed: true,
        }];

        let text = extract_text(&word_doc, &pieces, 3);
        assert_eq!(text, "Hel");
    }
}
