use std::io::{Read, Seek, SeekFrom};

use super::directory::{DirEntry, EntryType, parse_directory};
use super::error::{CfbError, Result};
use super::header::{CfbHeader, MAX_REG_SECT};

/// A reader for Compound Binary File (OLE2/CFBF) containers.
///
/// Provides random access to streams within the file.
pub struct CfbReader<R> {
    reader: R,
    header: CfbHeader,
    /// The full FAT: maps each sector → next sector in chain.
    fat: Vec<u32>,
    /// The mini-FAT: maps each mini-sector → next mini-sector.
    mini_fat: Vec<u32>,
    /// Directory entries.
    entries: Vec<DirEntry>,
    /// The mini-stream data (read from the root entry's stream chain).
    mini_stream: Vec<u8>,
}

impl<R: Read + Seek> CfbReader<R> {
    /// Open and parse a CFB file.
    pub fn new(mut reader: R) -> Result<Self> {
        // Read header.
        let mut header_buf = [0u8; 512];
        reader.read_exact(&mut header_buf)?;
        let header = CfbHeader::parse(&header_buf)?;

        // Build the FAT.
        let fat = Self::read_fat(&mut reader, &header)?;

        // Read directory entries.
        let dir_data = Self::read_chain(&mut reader, &header, &fat, header.first_dir_sector, None)?;
        crate::budget::charge_cfb_internal(dir_data.len() as u64).map_err(map_budget_error)?;
        if !dir_data.len().is_multiple_of(128) {
            return Err(CfbError::InvalidDirectory(
                "directory stream has a partial entry".into(),
            ));
        }
        crate::budget::check_cfb_entries(dir_data.len() / 128).map_err(map_budget_error)?;
        let entries = parse_directory(&dir_data, header.major_version)?;
        if entries.first().map(|entry| entry.entry_type) != Some(EntryType::RootStorage) {
            return Err(CfbError::InvalidDirectory(
                "missing root storage entry".into(),
            ));
        }
        validate_directory_tree(&entries)?;

        // Read mini-FAT.
        let mini_fat = if header.first_mini_fat_sector <= MAX_REG_SECT {
            let mini_fat_bytes = (header.mini_fat_sector_count as usize)
                .checked_mul(header.sector_size)
                .ok_or_else(|| CfbError::InvalidHeader("mini-FAT size overflow".into()))?;
            let mini_fat_data = Self::read_chain(
                &mut reader,
                &header,
                &fat,
                header.first_mini_fat_sector,
                Some(mini_fat_bytes),
            )?;
            crate::budget::charge_cfb_internal(mini_fat_data.len() as u64)
                .map_err(map_budget_error)?;
            crate::budget::charge_cfb_internal(mini_fat_data.len() as u64)
                .map_err(map_budget_error)?;
            mini_fat_data
                .as_chunks::<4>()
                .0
                .iter()
                .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect()
        } else {
            Vec::new()
        };

        // Read mini-stream (data from root entry's stream chain).
        let mini_stream = if !entries.is_empty()
            && entries[0].entry_type == EntryType::RootStorage
            && entries[0].start_sector <= MAX_REG_SECT
        {
            let size = usize::try_from(entries[0].stream_size)
                .map_err(|_| CfbError::CorruptedStream("mini-stream size overflow".into()))?;
            let data = Self::read_chain(
                &mut reader,
                &header,
                &fat,
                entries[0].start_sector,
                Some(size),
            )?;
            crate::budget::charge_cfb_internal(data.len() as u64).map_err(map_budget_error)?;
            data
        } else {
            Vec::new()
        };

        Ok(Self {
            reader,
            header,
            fat,
            mini_fat,
            entries,
            mini_stream,
        })
    }

    /// Find a stream entry by name (case-insensitive).
    pub fn find_entry(&self, name: &str) -> Option<usize> {
        let lower = name.to_ascii_lowercase();
        let mut stack = vec![self.entries.first()?.child];
        let mut visited = std::collections::HashSet::new();
        while let Some(index) = stack.pop() {
            if index == u32::MAX || !visited.insert(index) {
                continue;
            }
            let entry = self.entries.get(index as usize)?;
            stack.push(entry.left_sibling);
            stack.push(entry.right_sibling);
            if entry.entry_type == EntryType::Stream && entry.name.to_ascii_lowercase() == lower {
                return Some(index as usize);
            }
        }
        None
    }

    /// Read a stream by directory entry index.
    pub fn read_stream_by_index(&mut self, index: usize) -> Result<Vec<u8>> {
        let entry = self
            .entries
            .get(index)
            .ok_or_else(|| CfbError::StreamNotFound(format!("no entry at index {index}")))?;

        let size = usize::try_from(entry.stream_size)
            .map_err(|_| CfbError::CorruptedStream("stream size overflow".into()))?;
        crate::budget::charge_cfb_stream(entry.stream_size).map_err(map_budget_error)?;
        let start = entry.start_sector;

        if size == 0 {
            return Ok(Vec::new());
        }

        // Decide: regular stream or mini-stream?
        // Use mini-stream only if: size < cutoff, not root, and mini-stream exists.
        if size < self.header.mini_stream_cutoff as usize
            && entry.entry_type != EntryType::RootStorage
            && !self.mini_stream.is_empty()
        {
            self.read_mini_stream(start, size)
        } else {
            Self::read_chain(&mut self.reader, &self.header, &self.fat, start, Some(size))
        }
    }

    /// Open a stream by name (case-insensitive).
    pub fn open_stream(&mut self, name: &str) -> Result<Vec<u8>> {
        let idx = self
            .find_entry(name)
            .ok_or_else(|| CfbError::StreamNotFound(name.to_string()))?;
        self.read_stream_by_index(idx)
    }

    /// Check if a stream with the given name exists.
    pub fn has_stream(&self, name: &str) -> bool {
        self.find_entry(name).is_some()
    }

    // ── Internal helpers ──

    /// Build the complete FAT from DIFAT entries (header + DIFAT chain).
    fn read_fat(reader: &mut R, header: &CfbHeader) -> Result<Vec<u32>> {
        // Collect all FAT sector locations from DIFAT.
        let mut fat_sectors: Vec<u32> = header
            .header_difat
            .iter()
            .copied()
            .filter(|&s| s <= MAX_REG_SECT)
            .collect();
        crate::budget::check_cfb_entries(header.fat_sector_count as usize)
            .map_err(map_budget_error)?;
        crate::budget::check_cfb_entries(header.difat_sector_count as usize)
            .map_err(map_budget_error)?;

        // Follow the DIFAT chain for large files.
        let mut difat_sector = header.first_difat_sector;
        let mut visited_difat = std::collections::HashSet::new();
        let mut difat_count = 0_u32;
        let entries_per_difat = header.sector_size / 4 - 1; // last u32 is next DIFAT sector
        while difat_sector <= MAX_REG_SECT {
            crate::budget::check_cancelled().map_err(map_budget_error)?;
            if !visited_difat.insert(difat_sector) {
                return Err(CfbError::InvalidFat("DIFAT chain cycle detected".into()));
            }
            difat_count = difat_count.saturating_add(1);
            if difat_count > header.difat_sector_count {
                return Err(CfbError::InvalidFat(
                    "DIFAT chain exceeds declared sector count".into(),
                ));
            }
            let mut sector_buf = vec![0u8; header.sector_size];
            reader.seek(SeekFrom::Start(header.sector_offset(difat_sector)))?;
            let n = read_fully(reader, &mut sector_buf)?;
            if n != header.sector_size {
                return Err(CfbError::InvalidFat("truncated DIFAT sector".into()));
            }

            for i in 0..entries_per_difat {
                let off = i * 4;
                let val = u32::from_le_bytes([
                    sector_buf[off],
                    sector_buf[off + 1],
                    sector_buf[off + 2],
                    sector_buf[off + 3],
                ]);
                if val <= MAX_REG_SECT {
                    fat_sectors.push(val);
                }
            }

            // Next DIFAT sector.
            let next_off = entries_per_difat * 4;
            difat_sector = u32::from_le_bytes([
                sector_buf[next_off],
                sector_buf[next_off + 1],
                sector_buf[next_off + 2],
                sector_buf[next_off + 3],
            ]);
        }
        if difat_count != header.difat_sector_count {
            return Err(CfbError::InvalidFat(
                "DIFAT chain length does not match header".into(),
            ));
        }
        let unique_fat_sectors = fat_sectors
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>();
        if unique_fat_sectors.len() != fat_sectors.len() {
            return Err(CfbError::InvalidFat("duplicate FAT sector".into()));
        }
        if fat_sectors.len() != header.fat_sector_count as usize {
            return Err(CfbError::InvalidFat(
                "FAT sector count does not match header".into(),
            ));
        }

        // Read each FAT sector and concatenate entries.
        let entries_per_fat_sector = header.sector_size / 4;
        let fat_entries = fat_sectors
            .len()
            .checked_mul(entries_per_fat_sector)
            .ok_or_else(|| CfbError::InvalidHeader("FAT entry count overflow".into()))?;
        crate::budget::check_cfb_entries(fat_entries).map_err(map_budget_error)?;
        let fat_bytes = fat_entries
            .checked_mul(std::mem::size_of::<u32>())
            .ok_or_else(|| CfbError::InvalidHeader("FAT byte size overflow".into()))?;
        crate::budget::charge_cfb_internal(fat_bytes as u64).map_err(map_budget_error)?;
        let mut fat = Vec::with_capacity(fat_entries);
        let mut sector_buf = vec![0u8; header.sector_size];

        for &fat_sec in &fat_sectors {
            crate::budget::check_cancelled().map_err(map_budget_error)?;
            reader.seek(SeekFrom::Start(header.sector_offset(fat_sec)))?;
            let n = read_fully(reader, &mut sector_buf)?;
            if n != header.sector_size {
                return Err(CfbError::InvalidFat("truncated FAT sector".into()));
            }
            for i in 0..entries_per_fat_sector {
                let off = i * 4;
                fat.push(u32::from_le_bytes([
                    sector_buf[off],
                    sector_buf[off + 1],
                    sector_buf[off + 2],
                    sector_buf[off + 3],
                ]));
            }
        }

        Ok(fat)
    }

    /// Read a chain of sectors starting at `start` and return the concatenated data.
    fn read_chain(
        reader: &mut R,
        header: &CfbHeader,
        fat: &[u32],
        start: u32,
        expected_size: Option<usize>,
    ) -> Result<Vec<u8>> {
        if let Some(size) = expected_size {
            crate::budget::charge_cfb_growth(size as u64).map_err(map_budget_error)?;
        }
        let mut data = Vec::with_capacity(expected_size.unwrap_or(0));
        let mut sector = start;
        let mut visited = std::collections::HashSet::new();

        while sector <= MAX_REG_SECT && expected_size.is_none_or(|size| data.len() < size) {
            crate::budget::check_cancelled().map_err(map_budget_error)?;
            if !visited.insert(sector) {
                return Err(CfbError::CorruptedStream("FAT chain cycle detected".into()));
            }
            if sector as usize >= fat.len() {
                return Err(CfbError::CorruptedStream(
                    "FAT chain sector is out of range".into(),
                ));
            }

            let offset = header.sector_offset(sector);
            let mut buf = vec![0u8; header.sector_size];
            reader.seek(SeekFrom::Start(offset))?;
            let n = read_fully(reader, &mut buf)?;
            if n != header.sector_size {
                return Err(CfbError::CorruptedStream("truncated sector chain".into()));
            }
            let take = expected_size
                .map(|size| size.saturating_sub(data.len()).min(buf.len()))
                .unwrap_or(buf.len());
            let projected = data
                .len()
                .checked_add(take)
                .ok_or_else(|| CfbError::CorruptedStream("sector chain size overflow".into()))?;
            crate::budget::charge_cfb_growth(projected as u64).map_err(map_budget_error)?;
            data.extend_from_slice(&buf[..take]);

            // Follow chain.
            sector = fat[sector as usize];
        }

        if expected_size.is_some_and(|size| data.len() != size) {
            return Err(CfbError::CorruptedStream(
                "sector chain is shorter than declared stream size".into(),
            ));
        }
        if expected_size.is_some() && sector <= MAX_REG_SECT {
            return Err(CfbError::CorruptedStream(
                "sector chain is longer than declared stream size".into(),
            ));
        }

        Ok(data)
    }

    /// Read from the mini-stream using mini-FAT chain.
    fn read_mini_stream(&self, start: u32, size: usize) -> Result<Vec<u8>> {
        let mut data = Vec::with_capacity(size);
        let mut sector = start;
        let mut remaining = size;
        let mini_sector_size = self.header.mini_sector_size;
        let mut visited = std::collections::HashSet::new();

        while sector <= MAX_REG_SECT && remaining > 0 {
            crate::budget::check_cancelled().map_err(map_budget_error)?;
            if !visited.insert(sector) {
                return Err(CfbError::CorruptedStream(
                    "mini-FAT chain cycle detected".into(),
                ));
            }

            let offset = sector as usize * mini_sector_size;
            let to_read = remaining.min(mini_sector_size);

            let end = offset
                .checked_add(to_read)
                .ok_or_else(|| CfbError::CorruptedStream("mini-stream offset overflow".into()))?;
            let bytes = self
                .mini_stream
                .get(offset..end)
                .ok_or_else(|| CfbError::CorruptedStream("truncated mini-stream chain".into()))?;
            let projected = data
                .len()
                .checked_add(bytes.len())
                .ok_or_else(|| CfbError::CorruptedStream("mini-stream size overflow".into()))?;
            crate::budget::charge_cfb_growth(projected as u64).map_err(map_budget_error)?;
            data.extend_from_slice(bytes);

            remaining -= to_read;

            sector = *self.mini_fat.get(sector as usize).ok_or_else(|| {
                CfbError::CorruptedStream("mini-FAT sector is out of range".into())
            })?;
        }

        if remaining != 0 {
            return Err(CfbError::CorruptedStream(
                "mini-FAT chain is shorter than declared stream size".into(),
            ));
        }
        if sector <= MAX_REG_SECT {
            return Err(CfbError::CorruptedStream(
                "mini-FAT chain is longer than declared stream size".into(),
            ));
        }

        Ok(data)
    }
}

fn validate_directory_tree(entries: &[DirEntry]) -> Result<()> {
    if entries
        .iter()
        .skip(1)
        .any(|entry| entry.entry_type == EntryType::RootStorage)
    {
        return Err(CfbError::InvalidDirectory(
            "multiple root storage entries".into(),
        ));
    }
    let mut stack = vec![0_u32];
    let mut visited = std::collections::HashSet::new();
    while let Some(index) = stack.pop() {
        crate::budget::check_cancelled().map_err(map_budget_error)?;
        if index == u32::MAX {
            continue;
        }
        let entry = entries.get(index as usize).ok_or_else(|| {
            CfbError::InvalidDirectory("directory tree index is out of range".into())
        })?;
        if !visited.insert(index) {
            return Err(CfbError::InvalidDirectory(
                "directory tree cycle or duplicate reference".into(),
            ));
        }
        stack.push(entry.left_sibling);
        stack.push(entry.right_sibling);
        if matches!(
            entry.entry_type,
            EntryType::Storage | EntryType::RootStorage
        ) {
            stack.push(entry.child);
        } else if entry.child != u32::MAX {
            return Err(CfbError::InvalidDirectory(
                "non-storage directory entry has children".into(),
            ));
        }
    }
    if entries.iter().enumerate().any(|(index, entry)| {
        entry.entry_type != EntryType::Empty && !visited.contains(&(index as u32))
    }) {
        return Err(CfbError::InvalidDirectory(
            "orphaned directory entry".into(),
        ));
    }
    Ok(())
}

/// Read into `buf` while retaining cooperative cancellation checkpoints.
fn read_fully<R: Read>(reader: &mut R, buf: &mut [u8]) -> super::error::Result<usize> {
    let mut total = 0;
    while total < buf.len() {
        crate::budget::check_cancelled().map_err(map_budget_error)?;
        match reader.read(&mut buf[total..]) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(total)
}

fn map_budget_error(error: crate::OfficeReadError) -> CfbError {
    match error {
        crate::OfficeReadError::ResourceLimit {
            resource,
            limit,
            observed,
        } => CfbError::ResourceLimit {
            resource,
            limit,
            observed,
        },
        crate::OfficeReadError::Cancelled => CfbError::Cancelled,
        _ => CfbError::Cancelled,
    }
}
