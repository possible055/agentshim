use super::*;

/// Find the byte offset of the xref table by scanning from the end of the file.
///
/// Searches for the "startxref" keyword in the last portion of the file,
/// then extracts the offset that follows it.
///
/// # Errors
///
/// Returns `Error::InvalidXref` if:
/// - The "startxref" keyword is not found
/// - The offset following "startxref" cannot be parsed
/// - The file is too small to contain a valid xref reference
pub fn find_xref_offset<R: Read + Seek>(reader: &mut R) -> Result<u64> {
    let file_size = reader.seek(SeekFrom::End(0))?;

    // GROW the tail window until `startxref` is found. A fixed 2 KB window assumes
    // the keyword sits close to the end of the file - and real producers break that
    // assumption constantly by PADDING the file after `%%EOF`:
    //
    //   gov.uscourts.ca2.*  192 KiB exactly, with **5,245 trailing NUL bytes**.
    //
    // Those are valid, readable PDFs (poppler and Acrobat open them), but the last
    // 2 KB is nothing but padding, so `startxref` is not in the window and the whole
    // document is rejected as InvalidXref - a TOTAL loss, not a degradation. Growing
    // the window costs nothing on a well-formed file (the first 2 KB read hits) and
    // recovers the padded ones.
    const WINDOWS: [u64; 5] = [2048, 16 * 1024, 128 * 1024, 1024 * 1024, u64::MAX];
    for &want in WINDOWS.iter() {
        let read_size = std::cmp::min(want, file_size);
        reader.seek(SeekFrom::End(-(read_size as i64)))?;

        let mut buf = Vec::new();
        reader.take(read_size).read_to_end(&mut buf)?;
        let content = String::from_utf8_lossy(&buf);

        if let Some(pos) = content.rfind("startxref") {
            let after_keyword = &content[pos + 9..]; // 9 = len("startxref")

            // Split lines manually to handle CR, LF, and CRLF line endings.
            // Standard .lines() only handles LF and CRLF, not standalone CR.
            for line in split_lines(after_keyword) {
                // Trim NUL padding as well as whitespace: a padded file leaves the
                // offset line looking like "189089\0\0\0..." and `str::trim` alone
                // does not strip NULs, so the all-digits check below would fail.
                let trimmed = line.trim_matches(|c: char| c.is_whitespace() || c == '\0');
                if !trimmed.is_empty() && trimmed.chars().all(|c| c.is_ascii_digit()) {
                    return trimmed.parse::<u64>().map_err(|_| Error::InvalidXref);
                }
            }
            // Keyword found but no offset after it - a wider window will not help.
            return Err(Error::InvalidXref);
        }

        // Whole file already scanned: the keyword genuinely is not there.
        if read_size == file_size {
            break;
        }
    }

    Err(Error::InvalidXref)
}

/// Parse the cross-reference table at the given byte offset.
///
/// Automatically detects whether this is a traditional xref table or
/// a cross-reference stream (PDF 1.5+) and parses accordingly.
///
/// # Errors
///
/// Returns `Error::InvalidXref` if parsing fails for both formats.
pub fn parse_xref<R: Read + Seek>(reader: &mut R, offset: u64) -> Result<CrossRefTable> {
    parse_xref_iterative(reader, offset)
}

/// Extract /Length value from raw bytes of an xref stream object header.
///
/// Searches for `/Length` followed by an integer in the raw dictionary bytes.
/// Returns `None` if not found or not parseable. This avoids full object parsing
/// just to determine how much data to read.
pub(super) fn find_stream_length(data: &[u8]) -> Option<usize> {
    // Search for "/Length" (case-sensitive, per PDF spec)
    let keyword = b"/Length";
    let pos = data.windows(keyword.len()).position(|w| w == keyword)?;
    let after = &data[pos + keyword.len()..];

    // Skip whitespace
    let start = after.iter().position(|&b| !b.is_ascii_whitespace())?;
    let after = &after[start..];

    // If the next token is a digit, parse the integer
    if after.first()?.is_ascii_digit() {
        let end = after
            .iter()
            .position(|b| !b.is_ascii_digit())
            .unwrap_or(after.len());
        let num_str = std::str::from_utf8(&after[..end]).ok()?;
        num_str.parse::<usize>().ok()
    } else {
        // /Length is an indirect reference — we can't resolve it without full parsing
        None
    }
}

/// Try to find the actual xref start near the given offset.
///
/// Some PDF producers miscalculate the startxref offset by a few bytes.
/// This function scans a small window around the given offset to find either
/// the "xref" keyword (traditional table) or an object header like "N 0 obj"
/// (cross-reference stream). This tolerance is common in PDF readers (MuPDF,
/// poppler, etc.) because startxref misalignment is a well-known PDF producer bug.
pub(super) fn find_actual_xref_offset<R: Read + Seek>(reader: &mut R, offset: u64) -> Result<u64> {
    // First, check if the offset is already correct
    reader.seek(SeekFrom::Start(offset))?;
    let mut peek = [0u8; 64];
    let n = reader.read(&mut peek)?;
    let peek_str = String::from_utf8_lossy(&peek[..n]);
    let trimmed = peek_str.trim_start();
    if trimmed.starts_with("xref") || trimmed.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return Ok(offset);
    }

    // Offset is misaligned — scan a window around it.
    // We scan backward (up to 32 bytes) and forward (up to 64 bytes).
    const SCAN_BACK: u64 = 32;
    const SCAN_FWD: u64 = 64;
    let scan_start = offset.saturating_sub(SCAN_BACK);
    let scan_len = (SCAN_BACK + SCAN_FWD) as usize;

    reader.seek(SeekFrom::Start(scan_start))?;
    let mut buf = vec![0u8; scan_len];
    let bytes_read = reader.read(&mut buf)?;
    buf.truncate(bytes_read);

    // Look for "xref" keyword preceded by a line break or at buffer start
    for i in 0..bytes_read.saturating_sub(3) {
        if &buf[i..i + 4] == b"xref" {
            // Ensure it's at a line boundary (start of buffer, or preceded by CR/LF)
            if i == 0 || buf[i - 1] == b'\r' || buf[i - 1] == b'\n' {
                let found_offset = scan_start + i as u64;
                log::debug!(
                    "Corrected xref offset: {} -> {} (found 'xref' keyword)",
                    offset,
                    found_offset
                );
                return Ok(found_offset);
            }
        }
    }

    // Look for object header pattern at line boundaries: "\r<digits>" or "\n<digits>"
    // followed by " <gen> obj". This handles cross-reference streams.
    for i in 0..bytes_read {
        // Must be at a line boundary
        let at_line_start = i == 0 || buf[i - 1] == b'\r' || buf[i - 1] == b'\n';
        if !at_line_start || !buf[i].is_ascii_digit() {
            continue;
        }

        // Found a digit at a line boundary — check for "N N obj" pattern
        let remaining = &buf[i..bytes_read];
        let remaining_str = String::from_utf8_lossy(remaining);
        if let Some(obj_pos) = remaining_str.find(" obj") {
            let before_obj = &remaining_str[..obj_pos];
            let parts: Vec<&str> = before_obj.split_whitespace().collect();
            if parts.len() == 2
                && parts[0].chars().all(|c| c.is_ascii_digit())
                && parts[1].chars().all(|c| c.is_ascii_digit())
            {
                let found_offset = scan_start + i as u64;
                log::debug!(
                    "Corrected xref offset: {} -> {} (found object header '{} obj')",
                    offset,
                    found_offset,
                    before_obj.trim()
                );
                return Ok(found_offset);
            }
        }
    }

    // Could not find xref nearby — return original offset and let downstream handle the error
    log::debug!("Could not find xref near offset {}, using original", offset);
    Ok(offset)
}

/// Parse xref table iteratively, following /Prev pointers for incremental updates.
///
/// Uses a `HashSet` of visited offsets to detect circular /Prev chains instead of
/// an arbitrary depth limit. This supports PDFs with hundreds of incremental saves
/// (e.g., 177+ /Prev links) without falling back to expensive full-file reconstruction.
pub(super) fn parse_xref_iterative<R: Read + Seek>(
    reader: &mut R,
    start_offset: u64,
) -> Result<CrossRefTable> {
    let mut visited = HashSet::new();
    let mut offset = start_offset;
    let mut result_xref: Option<CrossRefTable> = None;

    loop {
        // Cycle detection: stop if we've already visited this offset
        if !visited.insert(offset) {
            log::warn!(
                "Circular /Prev chain detected at offset {}, stopping xref traversal",
                offset
            );
            break;
        }

        // Determine the actual xref offset, tolerating misalignment from PDF producers.
        let actual_offset = find_actual_xref_offset(reader, offset)?;

        reader.seek(SeekFrom::Start(actual_offset))?;

        // Peek at the first few bytes to determine xref type
        let mut peek_buf = [0u8; 64];
        let bytes_read = reader.read(&mut peek_buf)?;
        reader.seek(SeekFrom::Start(actual_offset))?;

        let peek_str = String::from_utf8_lossy(&peek_buf[..bytes_read]);
        let trimmed = peek_str.trim_start();

        log::debug!(
            "Parsing xref at offset {} (original: {}), peek: {:?} [chain depth: {}]",
            actual_offset,
            offset,
            crate::utils::safe_prefix(&peek_str, 15),
            visited.len()
        );

        // Parse the current xref (either traditional or stream)
        let xref = if trimmed.starts_with("xref") {
            log::debug!("Detected traditional xref at offset {}", actual_offset);
            parse_traditional_xref(reader, actual_offset)?
        } else if trimmed.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            match parse_xref_stream(reader, actual_offset) {
                Ok(xref) => xref,
                Err(e) => {
                    log::debug!("Failed to parse as xref stream: {}", e);
                    reader.seek(SeekFrom::Start(actual_offset))?;
                    match parse_traditional_xref(reader, actual_offset) {
                        Ok(xref) => xref,
                        Err(trad_err) => {
                            log::debug!("Failed to parse as traditional xref: {}", trad_err);
                            return Err(Error::InvalidPdf(format!(
                                "failed to parse xref (stream attempt: {}, traditional attempt: {})",
                                e, trad_err
                            )));
                        }
                    }
                }
            }
        } else {
            log::debug!(
                "Xref at offset {} starts with unexpected data: {:?}",
                actual_offset,
                crate::utils::safe_prefix(trimmed, 20)
            );
            return Err(Error::InvalidXref);
        };

        // Extract /Prev and /XRefStm pointers before `xref` is moved by the merge.
        let prev_offset = xref
            .trailer()
            .and_then(|t| t.get("Prev"))
            .and_then(|o| o.as_integer())
            .map(|v| v as u64);
        // Hybrid-reference files (§7.5.8.4): a classic trailer may carry
        // /XRefStm pointing to an xref stream that indexes the compressed
        // (object-stream) objects invisible to the classic table. Without this
        // the compressed objects are unreachable and the reader falls back to
        // full-file reconstruction.
        let xrefstm_offset = xref
            .trailer()
            .and_then(|t| t.get("XRefStm"))
            .and_then(|o| o.as_integer())
            .map(|v| v as u64);

        // Merge: most recent xref entries take priority over older ones
        match &mut result_xref {
            Some(result) => result.merge_from(xref),
            None => result_xref = Some(xref),
        }

        // Merge the /XRefStm supplement AFTER the classic table, so the classic
        // entries win for any object listed in both (per §7.5.8.4). Parsed
        // standalone (its own /Prev, if any, duplicates the classic chain we
        // already follow) and best-effort: a malformed supplement is skipped,
        // not fatal.
        if let Some(stm_off) = xrefstm_offset {
            if visited.insert(stm_off) {
                match find_actual_xref_offset(reader, stm_off)
                    .and_then(|actual| parse_xref_stream(reader, actual))
                {
                    Ok(stm_xref) => {
                        log::debug!("Merged /XRefStm supplement at offset {}", stm_off);
                        match &mut result_xref {
                            Some(result) => result.merge_from(stm_xref),
                            None => result_xref = Some(stm_xref),
                        }
                    }
                    Err(e) => {
                        log::debug!("Skipping unparseable /XRefStm at offset {}: {}", stm_off, e)
                    }
                }
            }
        }

        // Follow /Prev chain or stop
        match prev_offset {
            Some(prev) => {
                log::debug!(
                    "Following /Prev pointer to offset {} from xref at offset {}",
                    prev,
                    offset
                );
                offset = prev;
            }
            None => break,
        }
    }

    result_xref.ok_or(Error::InvalidXref)
}

/// Parse a traditional cross-reference table (PDF 1.0-1.4).
///
/// The xref table format is:
/// ```text
/// xref
/// 0 6             % Start at object 0, 6 entries
/// 0000000000 65535 f   % Object 0 (free)
/// 0000000018 00000 n   % Object 1 at byte 18
/// 0000000154 00000 n   % Object 2 at byte 154
/// ...
/// trailer
/// << /Size 6 /Root 1 0 R >>
/// ```
pub(super) fn parse_traditional_xref<R: Read + Seek>(
    reader: &mut R,
    offset: u64,
) -> Result<CrossRefTable> {
    log::debug!("parse_traditional_xref: Starting at offset {}", offset);
    reader.seek(SeekFrom::Start(offset))?;

    // Read only until "trailer" or "startxref" instead of the entire remaining file.
    // For linearized PDFs, the first xref may be near byte 0, and read_to_end would
    // load the entire file (e.g., 375MB) just to parse an 8-entry xref table.
    let lines = read_until_trailer(reader).map_err(|e| {
        log::error!("Failed to read xref lines: {}", e);
        Error::InvalidXref
    })?;

    log::debug!("parse_traditional_xref: Read {} lines", lines.len());

    let mut xref = CrossRefTable::new();
    let mut line_idx = 0;

    // Find "xref" keyword, skipping leading whitespace and stray data lines
    // Some PDFs have garbage bytes or comments before the xref keyword
    let mut skipped_lines = 0;
    const MAX_SKIP_LINES: usize = 10;
    while line_idx < lines.len() {
        let trimmed = lines[line_idx].trim();
        if trimmed.is_empty() {
            line_idx += 1;
            continue; // Skip empty lines (don't count toward limit)
        }
        if trimmed.starts_with("xref") {
            line_idx += 1;
            break; // Found xref keyword
        }
        // Tolerate a few unexpected lines before xref
        skipped_lines += 1;
        if skipped_lines > MAX_SKIP_LINES {
            return Err(Error::InvalidXref);
        }
        log::debug!("Skipping unexpected line before xref: {:?}", trimmed);
        line_idx += 1;
    }

    // Parse subsections
    while line_idx < lines.len() {
        let trimmed = lines[line_idx].trim();
        line_idx += 1;

        // End of xref table
        if trimmed.starts_with("trailer") {
            break;
        }

        // Skip empty lines and comments
        if trimmed.is_empty() || trimmed.starts_with('%') {
            continue;
        }

        // Parse subsection header: "start_obj count"
        let parts: Vec<&str> = trimmed.split_whitespace().collect();
        if parts.len() != 2 {
            continue; // Skip malformed lines
        }

        let start_obj: u32 = parts[0].parse().map_err(|_| Error::InvalidXref)?;
        let count: u32 = parts[1].parse().map_err(|_| Error::InvalidXref)?;

        // Validate reasonable count to prevent memory exhaustion
        if count > 1_000_000 {
            return Err(Error::InvalidPdf(
                "xref subsection count exceeds limit".to_string(),
            ));
        }

        // Parse entries in this subsection
        let mut i = 0;
        while i < count && line_idx < lines.len() {
            let trimmed = lines[line_idx].trim();
            line_idx += 1;

            // Skip empty lines (don't increment counter)
            if trimmed.is_empty() {
                continue;
            }

            // Check if we've hit the trailer (end of xref)
            if trimmed.starts_with("trailer") {
                // We expected more entries but hit trailer early
                log::warn!(
                    "Expected {} entries but only found {} before trailer",
                    count,
                    i
                );
                line_idx -= 1; // Back up so outer loop can process trailer
                break;
            }

            // Parse entry: "nnnnnnnnnn ggggg f/n"
            // Be flexible with whitespace and format
            let parts: Vec<&str> = trimmed.split_whitespace().collect();

            // Try to handle various malformed formats
            if parts.len() < 3 {
                // Try to parse with different separators or formats
                log::warn!(
                    "Malformed xref entry (too few parts) at index {}: {:?}",
                    i,
                    trimmed
                );

                // Still increment counter to maintain object numbering
                // Add a placeholder free entry to maintain object number sequence
                let entry = XRefEntry::free(0, 65535);
                xref.add_entry(start_obj + i, entry);
                i += 1;
                continue;
            }

            // Allow extra parts (some PDFs have trailing data)
            if parts.len() > 3 {
                log::debug!(
                    "XRef entry has {} parts (expected 3): {:?}",
                    parts.len(),
                    trimmed
                );
            }

            let offset: u64 = match parts[0].parse() {
                Ok(v) => v,
                Err(_) => {
                    log::warn!("Failed to parse offset at index {}: {:?}", i, parts[0]);
                    // Add free entry to maintain numbering
                    let entry = XRefEntry::free(0, 65535);
                    xref.add_entry(start_obj + i, entry);
                    i += 1;
                    continue;
                }
            };

            let generation: u16 = match parts[1].parse() {
                Ok(v) => v,
                Err(_) => {
                    log::warn!("Failed to parse generation at index {}: {:?}", i, parts[1]);
                    // Add free entry to maintain numbering
                    let entry = XRefEntry::free(0, 65535);
                    xref.add_entry(start_obj + i, entry);
                    i += 1;
                    continue;
                }
            };

            let type_flag = parts[2];

            // Validate type flag - be flexible with case and truncation
            let type_flag_normalized = type_flag.to_lowercase();
            let type_char = type_flag_normalized.chars().next().unwrap_or('?');

            let in_use = match type_char {
                'n' => true,
                'f' => false,
                _ => {
                    log::warn!(
                        "Invalid type flag at index {}: {:?}, treating as free",
                        i,
                        type_flag
                    );
                    // Treat as free entry instead of skipping
                    false
                }
            };

            let entry = XRefEntry::new(offset, generation, in_use);
            xref.add_entry(start_obj + i, entry);
            i += 1;
        }
    }

    // Parse the trailer dictionary from the remaining lines.
    // After the "trailer" keyword, the lines contain the trailer dict (e.g., "<< /Size 100 /Root 1 0 R /Prev 12345 >>").
    // We concatenate remaining lines and parse the dictionary so that /Prev and other
    // trailer entries are available via xref.trailer().
    let remaining_text: String = lines[line_idx..].join("\n");
    if !remaining_text.trim().is_empty() {
        // The trailer dict should start with "<<" after optional whitespace
        let trimmed = remaining_text.trim();
        if trimmed.starts_with("<<")
            || trimmed.starts_with("<< ")
            || trimmed.starts_with("<<\n")
            || trimmed.starts_with("<<\r")
        {
            if let Ok((_, trailer_obj)) = parse_object(trimmed.as_bytes()) {
                if let Some(dict) = trailer_obj.as_dict() {
                    xref.set_trailer(dict.clone());
                }
            }
        }
    }

    Ok(xref)
}
