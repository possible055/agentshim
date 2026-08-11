use super::*;

/// Parse a cross-reference stream (PDF 1.5+).
///
/// Cross-reference streams are stream objects with `/Type /XRef` that contain
/// binary encoded xref data. They replace traditional xref tables in modern PDFs.
///
/// The stream dictionary contains:
/// - `/W [w1 w2 w3]` - Field widths in bytes
/// - `/Size` - Total number of entries
/// - `/Index [start1 count1 start2 count2...]` - Optional subsection ranges
///
/// Each entry consists of 3 fields:
/// - Field 1: Entry type (0=free, 1=uncompressed, 2=compressed)
/// - Field 2: Offset (type 1) or stream object number (type 2)
/// - Field 3: Generation (type 1) or index within stream (type 2)
pub(super) fn parse_xref_stream<R: Read + Seek>(
    reader: &mut R,
    offset: u64,
) -> Result<CrossRefTable> {
    use crate::lexer::token;

    reader.seek(SeekFrom::Start(offset))?;

    // Read a bounded amount of data for the xref stream object.
    // We avoid read_to_end because for linearized PDFs the first xref may be
    // near the start of the file, and reading to end would load the entire file.
    //
    // Strategy: read an initial 256KB chunk, then check /Length to see if we
    // need more. Most xref streams are <64KB.
    let file_len = reader.seek(SeekFrom::End(0))?;
    reader.seek(SeekFrom::Start(offset))?;

    let remaining = (file_len - offset) as usize;
    let initial_read = remaining.min(256 * 1024);
    let mut content = vec![0u8; initial_read];
    let bytes_read = reader.read(&mut content)?;
    content.truncate(bytes_read);

    // Check if we need more data based on /Length or endobj presence
    let needs_more = if let Some(length_val) = find_stream_length(&content) {
        let stream_kw_pos = content.windows(6).position(|w| w == b"stream").unwrap_or(0);
        let needed = stream_kw_pos + 20 + length_val + 30;
        if needed > bytes_read {
            Some(needed)
        } else {
            None
        }
    } else if content.windows(6).any(|w| w == b"endobj") {
        None
    } else {
        // No /Length and no endobj in 256KB — read more (capped at 16MB)
        Some(remaining.min(16 * 1024 * 1024))
    };

    if let Some(needed) = needs_more {
        let total = needed.min(remaining);
        reader.seek(SeekFrom::Start(offset))?;
        content = vec![0u8; total];
        let mut total_read = 0;
        while total_read < total {
            let n = reader.read(&mut content[total_read..])?;
            if n == 0 {
                break;
            }
            total_read += n;
        }
        content.truncate(total_read);
    }

    // Parse the indirect object wrapper: "obj_num gen obj"
    let input = &content[..];

    // Skip object number
    let (rest, _obj_num_token) = token(input)
        .map_err(|e| Error::InvalidPdf(format!("failed to parse xref object number: {}", e)))?;

    // Skip generation number
    let (rest, _gen_token) = token(rest)
        .map_err(|e| Error::InvalidPdf(format!("failed to parse xref generation: {}", e)))?;

    // Skip 'obj' keyword
    let (rest, obj_keyword_token) = token(rest)
        .map_err(|e| Error::InvalidPdf(format!("failed to parse 'obj' keyword: {}", e)))?;

    // Verify it's actually the obj keyword
    if !matches!(obj_keyword_token, crate::lexer::Token::ObjStart) {
        return Err(Error::InvalidPdf(
            "expected 'obj' keyword in xref stream".to_string(),
        ));
    }

    // Now parse the actual object (should be a stream)
    let parse_result = parse_object(rest)
        .map_err(|e| Error::InvalidPdf(format!("failed to parse xref stream object: {}", e)))?;

    // Extract the Object from the IResult tuple (remaining_input, parsed_object)
    let (_remaining, obj) = parse_result;

    // Extract the stream dict and data
    let (stream_dict, stream_data) = match obj {
        Object::Stream { dict, data } => (dict, data),
        _ => {
            return Err(Error::InvalidPdf(
                "xref stream is not a stream object".to_string(),
            ))
        }
    };

    // Verify this is an xref stream
    if let Some(type_obj) = stream_dict.get("Type") {
        if let Some(type_name) = type_obj.as_name() {
            if type_name != "XRef" {
                return Err(Error::InvalidPdf(format!(
                    "expected /Type /XRef, got /Type /{}",
                    type_name
                )));
            }
        }
    }

    // Get field widths
    let w_array = stream_dict
        .get("W")
        .and_then(|o| o.as_array())
        .ok_or_else(|| Error::InvalidPdf("missing /W array in xref stream".to_string()))?;

    if w_array.len() != 3 {
        return Err(Error::InvalidPdf("invalid /W array length".to_string()));
    }

    let w1 = w_array[0]
        .as_integer()
        .ok_or_else(|| Error::InvalidPdf("invalid /W[0]".to_string()))? as usize;
    let w2 = w_array[1]
        .as_integer()
        .ok_or_else(|| Error::InvalidPdf("invalid /W[1]".to_string()))? as usize;
    let w3 = w_array[2]
        .as_integer()
        .ok_or_else(|| Error::InvalidPdf("invalid /W[2]".to_string()))? as usize;

    let entry_size = w1 + w2 + w3;
    if entry_size == 0 {
        return Err(Error::InvalidPdf("xref stream entry size is 0".to_string()));
    }

    // Get size
    let size = stream_dict
        .get("Size")
        .and_then(|o| o.as_integer())
        .ok_or_else(|| Error::InvalidPdf("missing /Size in xref stream".to_string()))?
        as u32;

    // Get index array (or default to [0 Size])
    let index_ranges = if let Some(index_obj) = stream_dict.get("Index") {
        let index_array = index_obj
            .as_array()
            .ok_or_else(|| Error::InvalidPdf("invalid /Index".to_string()))?;

        if index_array.len() % 2 != 0 {
            return Err(Error::InvalidPdf(
                "xref stream /Index array has odd length".to_string(),
            ));
        }
        let mut ranges = Vec::new();
        for i in (0..index_array.len()).step_by(2) {
            let start = index_array[i]
                .as_integer()
                .ok_or_else(|| Error::InvalidPdf("invalid index start".to_string()))?
                as u32;
            let count = index_array[i + 1]
                .as_integer()
                .ok_or_else(|| Error::InvalidPdf("invalid index count".to_string()))?
                as u32;
            ranges.push((start, count));
        }
        ranges
    } else {
        vec![(0, size)]
    };

    // Extract decode parameters if present
    let decode_params = if let Some(decode_params_obj) = stream_dict.get("DecodeParms") {
        extract_decode_params(decode_params_obj)?
    } else {
        None
    };

    // Decode the stream data
    let decoded_data = if let Some(filter_obj) = stream_dict.get("Filter") {
        let filter_name = match filter_obj {
            Object::Name(name) => name.clone(),
            Object::Array(arr) => {
                // Multiple filters - use first one for now (or chain them)
                if let Some(Object::Name(name)) = arr.first() {
                    name.clone()
                } else {
                    return Err(Error::InvalidPdf("invalid filter array".to_string()));
                }
            }
            _ => {
                return Err(Error::InvalidPdf(
                    "invalid /Filter in xref stream".to_string(),
                ))
            }
        };

        crate::decoders::decode_stream_with_params(
            &stream_data,
            &[filter_name],
            decode_params.as_ref(),
        )?
    } else {
        stream_data.to_vec()
    };

    // Parse the binary xref data
    let mut xref = CrossRefTable::new();
    let mut data_pos = 0;

    for (start_obj, count) in index_ranges {
        for i in 0..count {
            if data_pos + entry_size > decoded_data.len() {
                return Err(Error::InvalidPdf("truncated xref stream data".to_string()));
            }

            let entry_data = &decoded_data[data_pos..data_pos + entry_size];
            data_pos += entry_size;

            // Read field 1 (type)
            let entry_type = if w1 > 0 {
                read_int(&entry_data[0..w1])
            } else {
                1 // Default to type 1 if width is 0
            };

            // Read field 2
            let field2 = read_int(&entry_data[w1..w1 + w2]);

            // Read field 3
            let field3 = read_int(&entry_data[w1 + w2..w1 + w2 + w3]);

            let entry = match entry_type {
                0 => {
                    // Type 0: Free object
                    XRefEntry::free(field2, field3 as u16)
                }
                1 => {
                    // Type 1: Uncompressed object at byte offset
                    XRefEntry::uncompressed(field2, field3 as u16)
                }
                2 => {
                    // Type 2: Compressed object in object stream
                    XRefEntry::compressed(field2, field3 as u16)
                }
                _ => {
                    return Err(Error::InvalidPdf(format!(
                        "invalid xref entry type: {}",
                        entry_type
                    )));
                }
            };

            xref.add_entry(start_obj + i, entry);
        }
    }

    // For xref streams, the stream dictionary serves as the trailer
    xref.set_trailer(stream_dict);

    Ok(xref)
}

/// Extract decode parameters from a DecodeParms object.
///
/// DecodeParms can be either a dictionary or an array of dictionaries.
/// For simplicity, we only extract from the first dictionary if it's an array.
fn extract_decode_params(
    decode_params_obj: &Object,
) -> Result<Option<crate::decoders::DecodeParams>> {
    let dict = match decode_params_obj {
        Object::Dictionary(d) => d,
        Object::Array(arr) => {
            // For array of params, use first one
            if let Some(Object::Dictionary(d)) = arr.first() {
                d
            } else {
                return Ok(None);
            }
        }
        _ => return Ok(None),
    };

    let predictor = dict
        .get("Predictor")
        .and_then(|o| o.as_integer())
        .unwrap_or(1);

    let columns = dict
        .get("Columns")
        .and_then(|o| o.as_integer())
        .unwrap_or(1) as usize;

    let colors = dict.get("Colors").and_then(|o| o.as_integer()).unwrap_or(1) as usize;

    let bits_per_component = dict
        .get("BitsPerComponent")
        .and_then(|o| o.as_integer())
        .unwrap_or(8) as usize;

    Ok(Some(crate::decoders::DecodeParams {
        predictor,
        columns,
        colors,
        bits_per_component,
    }))
}

/// Read an integer from a byte slice (big-endian).
fn read_int(bytes: &[u8]) -> u64 {
    let mut result: u64 = 0;
    for &byte in bytes {
        result = (result << 8) | (byte as u64);
    }
    result
}

/// Split a string into lines, handling all PDF line ending styles (LF, CRLF, CR).
///
/// Standard .lines() only handles LF and CRLF, but some PDFs use
/// standalone CR (Mac-style line endings). This function handles all three.
pub(super) fn split_lines(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current_line = String::new();

    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        match chars[i] {
            '\r' => {
                // Check if next is \n (CRLF)
                if i + 1 < chars.len() && chars[i + 1] == '\n' {
                    // CRLF
                    lines.push(current_line.clone());
                    current_line.clear();
                    i += 2;
                } else {
                    // Just CR
                    lines.push(current_line.clone());
                    current_line.clear();
                    i += 1;
                }
            }
            '\n' => {
                // LF
                lines.push(current_line.clone());
                current_line.clear();
                i += 1;
            }
            ch => {
                current_line.push(ch);
                i += 1;
            }
        }
    }

    // Don't forget the last line if it doesn't end with a line ending
    if !current_line.is_empty() {
        lines.push(current_line);
    }

    lines
}

/// Read a line from a BufReader, handling all PDF line ending styles (LF, CRLF, CR).
///
/// Standard BufReader::read_line() only handles LF and CRLF, but some PDFs use
/// standalone CR (Mac-style line endings). This function handles all three by
/// reading the entire buffer and splitting manually.
/// Read the xref table and trailer from reader, bounded by finding "trailer" + dict.
///
/// This avoids `read_to_end` which would read the entire remaining file for
/// linearized PDFs where the first xref is near byte 0. Instead, we read in
/// chunks and search for the "trailer" keyword in raw bytes, which correctly
/// handles all line ending styles (CR, LF, CRLF).
pub(super) fn read_until_trailer<R: Read + Seek>(reader: &mut R) -> std::io::Result<Vec<String>> {
    // Read in chunks until we find "trailer" keyword followed by a dict,
    // followed by "startxref" or ">>" closing the dict.
    // Most xref tables are <1MB. We cap at 32MB to prevent runaway reads.
    const CHUNK_SIZE: usize = 256 * 1024;
    const MAX_TOTAL: usize = 32 * 1024 * 1024;

    let mut data = Vec::with_capacity(CHUNK_SIZE);
    let mut total_read = 0usize;
    let mut found_end = false;

    loop {
        let prev_len = data.len();
        data.resize(prev_len + CHUNK_SIZE, 0);
        let n = reader.read(&mut data[prev_len..])?;
        data.truncate(prev_len + n);
        total_read += n;

        if n == 0 {
            break; // EOF
        }

        // Search for the end of the trailer section: look for ">>" after "trailer"
        // then "startxref" or "%%EOF"
        if let Some(trailer_pos) = find_bytes(&data, b"trailer") {
            // Find the closing ">>" of the trailer dict after "trailer"
            let after_trailer = &data[trailer_pos + 7..];
            if let Some(dict_end) = find_bytes(after_trailer, b">>") {
                // Check if we also have "startxref" after ">>"
                let after_dict = &after_trailer[dict_end + 2..];
                if find_bytes(after_dict, b"startxref").is_some() || after_dict.len() > 20 {
                    found_end = true;
                    // Truncate to just past the trailer dict + a bit more
                    let end_pos = trailer_pos + 7 + dict_end + 2 + 50.min(after_dict.len());
                    data.truncate(end_pos);
                    break;
                }
            }
        }

        if total_read >= MAX_TOTAL {
            break;
        }
    }

    if !found_end {
        // Fallback: if we didn't find trailer end in 32MB, use what we have
        log::warn!(
            "Could not find trailer end marker within {}MB of xref",
            total_read / (1024 * 1024)
        );
    }

    // Split into lines handling CR, LF, and CRLF
    let text = String::from_utf8_lossy(&data);
    Ok(split_lines(&text))
}

/// Find the position of a byte pattern in a byte slice.
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
