use super::*;

/// Bytes read per scan step. Small enough that the buffer is negligible against the xref
/// rebuild budget, large enough that the regex amortises across a whole file.
const SCAN_CHUNK_BYTES: usize = 512 * 1024;
/// Carried between chunks so an object header straddling a boundary is still matched.
/// `4294967295 65535 obj` is 20 bytes; 64 leaves generous room for odd whitespace.
const SCAN_OVERLAP_BYTES: usize = 64;
/// Window read around a trailer keyword when parsing its dictionary.
const TRAILER_WINDOW_BYTES: usize = 64 * 1024;

pub(crate) struct ScanChunk {
    pub(crate) bytes: Vec<u8>,
    base: usize,
    /// Matches starting before this were already reported by the previous chunk.
    fresh_from: usize,
}

impl ScanChunk {
    /// Absolute file offset for a match, or `None` if it lies in the replayed overlap.
    pub(crate) fn absolute(&self, start: usize) -> Option<usize> {
        (start >= self.fresh_from).then_some(self.base + start)
    }
}

/// Streams a file in overlapping windows.
///
/// Reconstruction is reached from ordinary opening whenever `startxref` is damaged, so
/// it is input-triggered. Reading the file whole made a second complete copy of
/// attacker-controlled data — the one place the reader ever did — and ran a regex over
/// it. Windowing keeps the peak at one chunk regardless of file size.
pub(crate) struct ScanChunks<'reader, R: Read + Seek> {
    reader: &'reader mut R,
    buffer: Vec<u8>,
    overlap: usize,
    base: usize,
    carried: usize,
    finished: bool,
}

impl<'reader, R: Read + Seek> ScanChunks<'reader, R> {
    pub(crate) fn new(reader: &'reader mut R) -> Result<Self> {
        Self::with_overlap(reader, SCAN_OVERLAP_BYTES)
    }

    /// `overlap` must cover the longest lookahead the caller performs from a match, or a
    /// pattern near a window boundary would be read against truncated bytes.
    pub(crate) fn with_overlap(reader: &'reader mut R, overlap: usize) -> Result<Self> {
        crate::budget::check_xref_rebuild(SCAN_CHUNK_BYTES + overlap)?;
        reader.seek(SeekFrom::Start(0))?;
        Ok(Self {
            reader,
            buffer: Vec::with_capacity(SCAN_CHUNK_BYTES + overlap),
            overlap,
            base: 0,
            carried: 0,
            finished: false,
        })
    }
}

impl<R: Read + Seek> Iterator for ScanChunks<'_, R> {
    type Item = Result<ScanChunk>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        if let Err(error) = crate::budget::check_cancelled() {
            self.finished = true;
            return Some(Err(error));
        }

        // Keep the tail of the previous window so a straddling match is still visible.
        let carried = self.carried;
        if carried > 0 {
            let tail_start = self.buffer.len() - carried;
            self.buffer.copy_within(tail_start.., 0);
        }
        self.buffer.truncate(carried);

        let mut filled = carried;
        let mut scratch = [0_u8; 64 * 1024];
        while filled < SCAN_CHUNK_BYTES {
            let want = scratch.len().min(SCAN_CHUNK_BYTES - filled);
            match self.reader.read(&mut scratch[..want]) {
                Ok(0) => {
                    self.finished = true;
                    break;
                }
                Ok(read) => {
                    self.buffer.extend_from_slice(&scratch[..read]);
                    filled += read;
                }
                Err(error) => {
                    self.finished = true;
                    return Some(Err(Error::Io(error)));
                }
            }
        }
        if self.buffer.len() <= carried && self.finished {
            return None;
        }

        let chunk = ScanChunk {
            bytes: self.buffer.clone(),
            base: self.base.saturating_sub(carried),
            fresh_from: carried,
        };
        self.base = chunk.base + self.buffer.len();
        self.carried = if self.finished {
            0
        } else {
            self.overlap.min(self.buffer.len())
        };
        Some(Ok(chunk))
    }
}

/// Read a bounded window starting at `offset`, for parsing one trailer dictionary.
fn read_window<R: Read + Seek>(reader: &mut R, offset: usize, bytes: usize) -> Result<Vec<u8>> {
    crate::budget::check_xref_rebuild(bytes)?;
    reader.seek(SeekFrom::Start(offset as u64))?;
    let mut window = Vec::new();
    reader.take(bytes as u64).read_to_end(&mut window)?;
    Ok(window)
}

/// Find and parse the trailer dictionary.
///
/// Searches for "trailer" keyword in the file and attempts to parse the
/// dictionary that follows. If not found, attempts to reconstruct a minimal
/// trailer by finding the catalog object.
pub(super) fn find_trailer<R: Read + Seek>(
    trailer_offsets: &[usize],
    reader: &mut R,
    xref: &CrossRefTable,
) -> Result<(Object, Vec<(ObjectRef, Object)>)> {
    log::debug!("Searching for trailer dictionary...");

    // Search for all "trailer" keywords and prefer the last valid one.
    // Per ISO 32000-1:2008 Section 7.5.5, the most recent trailer (from the
    // latest incremental update) takes precedence. Using the first trailer can
    // miss /Encrypt entries added in later revisions.
    // The chosen /Root-bearing trailer plus the byte offset it was parsed
    // from (RE_TRAILER yields matches in ascending file order, so a later
    // offset = a more recent incremental update).
    let mut best_trailer: Option<(Object, usize)> = None;
    // /Encrypt /ID /Info salvaged from /Root-less parsed trailers, each
    // tracked with the offset it came from. If no /Root-bearing trailer
    // exists and we synthesize a minimal one, an encrypted file's /Encrypt
    // (and /ID, used for the encryption key) would otherwise be lost, making
    // the document undecryptable. Per ISO 32000-1 §7.5.5 the most recent
    // occurrence wins — including over a /Root-bearing trailer that appears
    // earlier in the file.
    let mut salvaged: HashMap<String, (Object, usize)> = HashMap::new();
    for trailer_start in trailer_offsets.iter().copied() {
        log::debug!("Found trailer keyword at offset {}", trailer_start);

        let trailer_keyword_end = trailer_start + 7; // len("trailer")
                                                     // A trailer dictionary is small; reading a bounded window keeps this path from
                                                     // reintroducing the whole-file buffer the scan just removed.
        let window = read_window(reader, trailer_keyword_end, TRAILER_WINDOW_BYTES)?;
        let input = window.as_slice();
        match parse_object(input) {
            Ok((_, obj)) => {
                // Only accept a parsed trailer that actually carries /Root.
                // A Linearized file's sparse end-of-file trailer legitimately
                // omits /Root — the Catalog is reachable via the linearization
                // parameters / first xref chain, not the trailing trailer
                // (issue #509). Accepting a /Root-less trailer here would
                // short-circuit Catalog discovery and fail downstream with
                // "Trailer missing /Root entry". The *last* /Root-bearing
                // trailer still wins for /Root itself.
                if obj.as_dict().is_some_and(|d| d.get("Root").is_some()) {
                    best_trailer = Some((obj, trailer_start));
                } else {
                    if let Some(d) = obj.as_dict() {
                        for key in ["Encrypt", "ID", "Info"] {
                            if let Some(v) = d.get(key) {
                                salvaged.insert(key.to_string(), (v.clone(), trailer_start));
                            }
                        }
                    }
                    log::debug!(
                        "Parsed trailer at offset {} has no /Root — skipping (Catalog located by object scan; /Encrypt /ID /Info preserved)",
                        trailer_start
                    );
                }
            }
            Err(e) => {
                log::warn!(
                    "Failed to parse trailer dictionary at offset {}: {}",
                    trailer_start,
                    e
                );
            }
        }
    }
    if let Some((mut trailer, best_off)) = best_trailer {
        // Merge salvaged /Encrypt /ID /Info from /Root-less trailers using
        // most-recent-occurrence-wins (ISO 32000-1 §7.5.5): a salvaged value
        // overrides the /Root-bearing trailer's only when it was parsed from
        // a *later* offset (a newer incremental update — e.g. a sparse
        // trailer that adds encryption or rotates the file ID), and always
        // fills a key the /Root-bearing trailer lacks. An earlier /Root-less
        // value never clobbers a newer explicit one.
        if !salvaged.is_empty() {
            if let Object::Dictionary(d) = &mut trailer {
                for (key, (value, off)) in &salvaged {
                    match d.get(key) {
                        Some(_) if *off <= best_off => {} // existing is newer/equal
                        _ => {
                            d.insert(key.clone(), value.clone());
                        }
                    }
                }
            }
        }
        log::info!("Successfully parsed trailer dictionary (last /Root-bearing occurrence)");
        // A parsed /Root-bearing trailer needs no synthesis.
        return Ok((trailer, Vec::new()));
    }

    // No /Root-bearing trailer found — synthesize one by scanning objects
    // for /Type /Catalog (handles Linearized files whose only trailer is the
    // sparse, /Root-less end-of-file trailer).
    log::info!(
        "No /Root-bearing trailer found; reconstructing minimal trailer via Catalog scan..."
    );
    let salvaged_values: HashMap<String, Object> =
        salvaged.into_iter().map(|(k, (v, _))| (k, v)).collect();
    reconstruct_minimal_trailer(reader, xref, &salvaged_values)
}
