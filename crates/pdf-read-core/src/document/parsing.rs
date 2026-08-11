use super::preflight::*;
use super::*;

/// Reference to an extracted image file.
///
/// Contains metadata about an image that has been extracted and saved to a file.
/// Used for HTML export to embed images with correct dimensions and format.
#[derive(Debug, Clone, PartialEq)]
pub struct ExtractedImageRef {
    /// Filename of the saved image (e.g., "img_001.png")
    pub filename: String,
    /// Image format
    pub format: ImageFormat,
    /// Image width in pixels
    pub width: u32,
    /// Image height in pixels
    pub height: u32,
    /// Bounding box in PDF user space (v0.3.14)
    pub bbox: Option<crate::geometry::Rect>,
    /// Rotation in degrees (v0.3.14)
    pub rotation: i32,
    /// Transformation matrix (v0.3.14)
    pub matrix: [f32; 6],
}

/// Image format for extracted images.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    /// PNG format (lossless)
    Png,
    /// JPEG format (lossy, preserves DCT-encoded images)
    Jpeg,
}

/// Extract the /Root reference from a trailer dictionary.
pub(super) fn get_root_ref_from_trailer(trailer: &Object) -> Option<ObjectRef> {
    trailer.as_dict()?.get("Root")?.as_reference()
}

/// First in-use *uncompressed* object in the xref, used as a /Root-independent
/// probe for the garbage-prefix offset-shift decision. Compressed
/// entries can't be seek-validated, so they're skipped.
pub(super) fn first_in_use_uncompressed(xref: &crate::xref::CrossRefTable) -> Option<ObjectRef> {
    xref.all_object_numbers()
        .filter_map(|n| xref.get(n).map(|e| (n, e)))
        .find(|(_, e)| e.in_use && e.entry_type == crate::xref::XRefEntryType::Uncompressed)
        .map(|(n, e)| ObjectRef::new(n, e.generation))
}

/// Heuristic: does this candidate table actually look like wrapped prose
/// clustered into x-columns rather than a real grid?
///
/// Cell contents in real data tables are atomic units (numbers, codes,
/// names, short labels): they almost always start with an uppercase
/// letter, a digit, or a symbol (currency, +/-, punctuation marker)
/// rarely end with a mid-sentence comma or semicolon. Prose-as-table
/// cells, by contrast, are fragments of running sentences — they
/// frequently start with a lowercase stopword ("and", "the", "to") because
/// the column boundary fell mid-clause, and frequently end with `,` or
/// `;` for the same reason.
///
/// We reject the candidate when either signal exceeds its threshold:
///   • > 12 % of cells end in `,` or `;` (mid-sentence tails), or
///   • > 25 % of cells start with a lowercase ASCII letter
///     (continuation fragments).
///
/// Thresholds chosen to clear the false positives flagged in the 88-PDF
/// regression (`searchable.pdf`, the WFMYY press-release, several arxiv
/// preprints) without disturbing legitimate data tables — sailing scores,
/// IRS forms, and the CJK traffic-volume grid all stay well below both
/// bars.
pub(super) fn looks_like_prose_table(table: &crate::structure::Table) -> bool {
    let mut total = 0usize;
    let mut sentence_tails = 0usize;
    let mut lower_starts = 0usize;
    let mut leader_dots = 0usize;
    for row in &table.rows {
        for cell in &row.cells {
            let trimmed = cell.text.trim();
            if trimmed.is_empty() {
                continue;
            }
            total += 1;
            if let Some(last) = trimmed.chars().last() {
                if matches!(last, ',' | ';') {
                    sentence_tails += 1;
                }
            }
            if let Some(first) = trimmed.chars().next() {
                if first.is_ascii_lowercase() {
                    lower_starts += 1;
                }
            }
            // Table-of-contents leader runs (". . . . . . ." between an
            // entry's title and its page number) cluster into their own
            // x-columns and create phantom 10–12-column "tables" out of
            // an ordinary three-column TOC. A cell whose content is
            // exclusively dots and spaces is the leader, not data.
            if trimmed.chars().all(|c| c == '.' || c == ' ') {
                leader_dots += 1;
            }
        }
    }
    if total < 10 {
        return false;
    }
    let tail_ratio = sentence_tails as f32 / total as f32;
    let lower_ratio = lower_starts as f32 / total as f32;
    let leader_ratio = leader_dots as f32 / total as f32;
    tail_ratio > 0.12 || lower_ratio > 0.25 || leader_ratio > 0.10
}

/// Check whether the object at the xref offset for `obj_ref` looks like a valid header.
pub(super) fn validate_object_at_offset<R: Read + Seek>(
    reader: &mut R,
    xref: &crate::xref::CrossRefTable,
    obj_ref: ObjectRef,
) -> bool {
    let entry = match xref.get(obj_ref.id) {
        Some(e) => e,
        None => return false,
    };
    // Compressed objects live inside object streams — their "offset" is the
    // stream object number, not a byte position. We cannot validate them by
    // seeking, but their presence in a correctly parsed xref stream is
    // sufficient proof that the xref is valid.
    if entry.entry_type == crate::xref::XRefEntryType::Compressed {
        return true;
    }
    if reader.seek(SeekFrom::Start(entry.offset)).is_err() {
        return false;
    }
    let mut buf = [0u8; 32];
    let n = reader.read(&mut buf).unwrap_or(0);
    if n == 0 {
        return false;
    }
    let s = String::from_utf8_lossy(&buf[..n]);
    // A valid object header starts with "N G obj"
    let mut parts = s.split_whitespace();
    // first token should be a number (obj id)
    let first_is_num = parts.next().is_some_and(|t| t.parse::<u32>().is_ok());
    let second_is_num = parts.next().is_some_and(|t| t.parse::<u16>().is_ok());
    let third_is_obj = parts
        .next()
        .is_some_and(|t| t == "obj" || t.starts_with("obj"));
    first_is_num && second_is_num && third_is_obj
}

/// Validate that the /Root catalog object is loadable from the xref.
pub(super) fn validate_root_loadable<R: Read + Seek>(
    reader: &mut R,
    xref: &crate::xref::CrossRefTable,
    trailer: &Object,
) -> bool {
    let root_ref = match get_root_ref_from_trailer(trailer) {
        Some(r) => r,
        None => return false, // No /Root at all — can't validate
    };
    validate_object_at_offset(reader, xref, root_ref)
}

/// Check if a string contains the standalone "obj" keyword (not "endobj").
///
/// This is used during multi-line object header parsing to detect when we've
/// accumulated enough lines to have a complete header. A naive `contains("obj")`
/// would match "endobj" and cause the loop to exit prematurely.
pub(super) fn has_standalone_obj_keyword(s: &str) -> bool {
    for (i, _) in s.match_indices("obj") {
        // Skip "endobj" — check if preceded by "end"
        if i >= 3 && &s[i - 3..i] == "end" {
            continue;
        }
        // Must be at a word boundary: preceded by whitespace, digit, or start of string
        if i == 0
            || s.as_bytes()[i - 1].is_ascii_whitespace()
            || s.as_bytes()[i - 1].is_ascii_digit()
        {
            return true;
        }
    }
    false
}

/// Parse PDF header (%PDF-x.y) from a reader.
///
/// # Arguments
///
/// * `reader` - A readable and seekable source (e.g., File, Cursor)
/// * `lenient` - If false, fail if header not at byte 0; if true, search first 8192 bytes
///
/// # Returns
///
/// Returns `Ok((major, minor, offset))` with the PDF version and byte offset where header was found.
/// In strict mode, offset will be 0 if successful. In lenient mode, offset may be > 0 for PDFs
/// with leading binary data (compliant with ISO 32000-1:2008, page 41).
///
/// # Examples
///
/// ```rust
/// use std::io::Cursor;
/// # use pdf_oxide::document::parse_header;
///
/// let data = b"%PDF-1.7\n";
/// let mut cursor = Cursor::new(data);
/// let (major, minor, offset) = parse_header(&mut cursor, false).unwrap();
/// assert_eq!((major, minor, offset), (1, 7, 0));
/// ```
pub fn parse_header<R: Read + Seek>(reader: &mut R, lenient: bool) -> Result<(u8, u8, u64)> {
    // Try to get current position
    let start_pos = reader.stream_position().unwrap_or(0);

    // Read first 8 bytes for fast path (header at byte 0)
    let mut header = [0u8; 8];
    let strict_read_ok = match reader.read_exact(&mut header) {
        Ok(_) => {
            // Check if header is at position 0
            if &header[0..5] == b"%PDF-" {
                return parse_version_from_header(&header, lenient)
                    .map(|(major, minor)| (major, minor, 0));
            }
            true
        }
        Err(e) => {
            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                // File too short for PDF header
                if !lenient {
                    return Err(Error::InvalidHeader(
                        "File too short for PDF header (expected at least 8 bytes)".to_string(),
                    ));
                }
                false
            } else {
                return Err(Error::InvalidHeader(format!("Failed to read file: {}", e)));
            }
        }
    };

    // If strict mode and first 8 bytes read, fail immediately
    if !lenient && strict_read_ok {
        return Err(Error::InvalidHeader(format!(
            "Expected '%PDF-' at byte 0, found '{}'",
            String::from_utf8_lossy(&header[0..5])
        )));
    }

    // Lenient mode: search first 8192 bytes
    reader.seek(SeekFrom::Start(start_pos))?;

    // Read up to 8192 bytes
    let mut buffer = vec![0u8; 8192];
    let bytes_read = match reader.read(&mut buffer) {
        Ok(0) => {
            return Err(Error::InvalidHeader(
                "File is empty (0 bytes read)".to_string(),
            ))
        }
        Ok(n) => n,
        Err(e) => {
            return Err(Error::InvalidHeader(format!(
                "I/O error while searching for PDF header: {}",
                e
            )))
        }
    };

    buffer.truncate(bytes_read);

    // Search for "%PDF-" marker
    match find_substring(&buffer, b"%PDF-") {
        Some(offset) => {
            // Verify we have enough bytes for the version
            if offset + 8 > buffer.len() {
                return Err(Error::InvalidHeader(
                    "PDF header found but insufficient bytes for version".to_string(),
                ));
            }

            let header_bytes = &buffer[offset..offset + 8];
            let mut header_arr = [0u8; 8];
            header_arr.copy_from_slice(header_bytes);

            let (major, minor) = parse_version_from_header(&header_arr, true)?;

            // Standardize reader position to just after the header
            // (consistent with strict mode behavior at line 4378)
            let header_start = start_pos + offset as u64;
            let after_header = header_start + 8;
            reader.seek(SeekFrom::Start(after_header))?;

            Ok((major, minor, header_start))
        }
        None => {
            if lenient {
                // Some PDFs lack a %PDF- header entirely (e.g., start with a binary
                // comment like %\xe2\xe3\xcf\xd3). Default to version 1.4.
                log::warn!("No %PDF- header found; assuming version 1.4 in lenient mode");
                reader.seek(SeekFrom::Start(0))?;
                Ok((1, 4, 0))
            } else {
                Err(Error::InvalidHeader(
                    "No PDF header found in first 8192 bytes of file".to_string(),
                ))
            }
        }
    }
}

/// Parse version information from a header buffer.
/// Assumes buffer starts with "%PDF-" and has at least 8 bytes.
///
/// When `lenient` is true, malformed version strings (e.g., `%PDF-1.\n`, `%PDF-a.4`)
/// default to version (1, 4) instead of returning an error.
pub(super) fn parse_version_from_header(header: &[u8; 8], lenient: bool) -> Result<(u8, u8)> {
    // Check magic bytes "%PDF-"
    if &header[0..5] != b"%PDF-" {
        return Err(Error::InvalidHeader(format!(
            "Expected '%PDF-', found '{}'",
            String::from_utf8_lossy(&header[0..5])
        )));
    }

    // Parse version (e.g., "1.7")
    // Format: %PDF-M.m where M is major version (1 digit), m is minor version (1 digit)
    if header[6] != b'.' {
        if lenient {
            log::warn!(
                "Malformed PDF version format (expected '.', found '{}'), defaulting to 1.4",
                header[6] as char
            );
            return Ok((1, 4));
        }
        return Err(Error::InvalidHeader(format!(
            "Invalid version format: expected '.', found '{}'",
            header[6] as char
        )));
    }

    let major = header[5];
    let minor = header[7];

    // Validate digits
    if !major.is_ascii_digit() || !minor.is_ascii_digit() {
        if lenient {
            log::warn!(
                "Malformed PDF version '{}.{}' (non-digit characters), defaulting to 1.4",
                major as char,
                minor as char
            );
            return Ok((1, 4));
        }
        return Err(Error::InvalidHeader(format!(
            "Invalid version: {}.{} (not digits)",
            major as char, minor as char
        )));
    }

    let major = major - b'0';
    let minor = minor - b'0';

    // Validate version range (PDF 1.0 - 2.0)
    if major > 2 || (major == 0 && minor == 0) {
        if lenient {
            log::warn!(
                "Unsupported PDF version {}.{}, defaulting to 1.4",
                major,
                minor
            );
            return Ok((1, 4));
        }
        return Err(Error::UnsupportedVersion(format!("{}.{}", major, minor)));
    }

    Ok((major, minor))
}

/// Parse the trailer dictionary from a reader.
///
/// The trailer comes immediately after the xref table and before "startxref".
/// It starts with the keyword "trailer" followed by a dictionary.
///
/// # Example Format
///
/// ```text
/// trailer
/// << /Size 6 /Root 1 0 R /Info 5 0 R >>
/// startxref
/// 1234
/// %%EOF
/// ```
///
/// # Arguments
///
/// * `reader` - A readable source positioned after the xref table
///
/// # Returns
///
/// Returns the trailer dictionary as an `Object`.
///
/// # Errors
///
/// Returns an error if:
/// - The "trailer" keyword is not found
/// - The dictionary following "trailer" cannot be parsed
/// - The reader encounters an I/O error
pub fn parse_trailer<R: Read>(reader: &mut R) -> Result<Object> {
    // The reader should already be positioned after the xref table
    // We need to read until we find "trailer", then parse the dictionary

    // Bounded rather than to EOF: the reader sits after the xref table, which in a
    // linearized file is near byte 0, so reading to the end would buffer almost the whole
    // document to find a dictionary that is a few hundred bytes long.
    let mut buffer = Vec::new();
    reader
        .take(TRAILER_SCAN_BYTES as u64)
        .read_to_end(&mut buffer)?;

    // Find "trailer" keyword
    let content = String::from_utf8_lossy(&buffer);
    let trailer_pos = content.find("trailer").ok_or_else(|| {
        Error::InvalidPdf("Trailer keyword not found after xref table".to_string())
    })?;

    // Skip past "trailer" keyword (7 bytes)
    let dict_start = trailer_pos + 7;
    if dict_start >= buffer.len() {
        return Err(Error::UnexpectedEof);
    }

    // Parse the dictionary that follows
    let (_, trailer_dict) = parse_object(&buffer[dict_start..]).map_err(|e| Error::ParseError {
        offset: dict_start,
        reason: format!("Failed to parse trailer dictionary: {:?}", e),
    })?;

    // Verify it's a dictionary
    if trailer_dict.as_dict().is_none() {
        return Err(Error::InvalidPdf("Trailer is not a dictionary".to_string()));
    }

    Ok(trailer_dict)
}

/// Find the first occurrence of a substring in a byte slice.
///
/// Returns the index of the first occurrence, or None if not found.
pub(super) fn find_substring(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }

    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Record every `N G obj` header in one scan window.
///
/// Split out of `scan_for_object` so the same byte-level matching runs over bounded
/// windows instead of a whole-file buffer. Matches that begin inside the replayed
/// overlap are skipped by `ScanChunk::absolute`, so a header straddling a window
/// boundary is recorded exactly once.
pub(super) fn scan_object_headers(
    content: &[u8],
    chunk: &crate::xref_reconstruction::ScanChunk,
    offsets: &mut HashMap<u32, u64>,
) {
    let mut pos = 0;
    while pos < content.len() {
        // Look for digit at a line start (after newline or at file start)
        let valid_start = pos == 0 || content[pos - 1] == b'\n' || content[pos - 1] == b'\r';
        if !valid_start || !content[pos].is_ascii_digit() {
            pos += 1;
            continue;
        }

        // Try to parse "N G obj" starting at pos
        let start = pos;
        // Parse object number (digits)
        while pos < content.len() && content[pos].is_ascii_digit() {
            pos += 1;
        }
        if pos >= content.len() || content[pos] != b' ' {
            continue;
        }
        let obj_num_str = std::str::from_utf8(&content[start..pos]).unwrap_or("");
        let obj_num: u32 = match obj_num_str.parse() {
            Ok(n) => n,
            Err(_) => continue,
        };

        pos += 1; // skip space

        // Parse generation number (digits)
        let gen_start = pos;
        while pos < content.len() && content[pos].is_ascii_digit() {
            pos += 1;
        }
        if pos >= content.len() || content[pos] != b' ' {
            continue;
        }
        let _gen_str = std::str::from_utf8(&content[gen_start..pos]).unwrap_or("");

        pos += 1; // skip space

        // Check for "obj" keyword
        if pos + 3 <= content.len() && &content[pos..pos + 3] == b"obj" {
            let after_obj = pos + 3;
            // Verify "obj" is followed by whitespace, newline, or '<'
            let valid_end = after_obj >= content.len() || {
                let c = content[after_obj];
                c == b'\n' || c == b'\r' || c == b' ' || c == b'\t' || c == b'<'
            };
            if valid_end {
                if let Some(absolute) = chunk.absolute(start) {
                    offsets.entry(obj_num).or_insert(absolute as u64);
                }
                pos = after_obj;
                continue;
            }
        }
        // Reset pos to just after the start to avoid infinite loop
        pos = start + 1;
    }
}
