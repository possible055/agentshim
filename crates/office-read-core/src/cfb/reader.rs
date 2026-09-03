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
                .chunks_exact(4)
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

#[cfg(any())]
mod tests {
    use super::*;
    use crate::cfb::header::{END_OF_CHAIN, FAT_SECT, FREE_SECT};
    use std::io::Cursor;

    /// Build a complete minimal CFB v3 file in memory with one stream.
    ///
    /// Layout (512-byte sectors):
    /// - Header (512 bytes)
    /// - Sector 0: Directory (4 entries × 128 bytes = 512 bytes)
    /// - Sector 1: FAT (128 entries × 4 bytes = 512 bytes)
    /// - Sector 2: Stream data ("Hello, CFB!")
    fn build_minimal_cfb() -> Vec<u8> {
        let sector_size = 512usize;

        // We'll have 3 sectors.
        let mut file = vec![0u8; 512 + 3 * sector_size]; // header + 3 sectors

        // ── Header ──
        // Signature
        file[0..8].copy_from_slice(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]);
        // Minor version
        file[0x18..0x1A].copy_from_slice(&0x003Eu16.to_le_bytes());
        // Major version = 3
        file[0x1A..0x1C].copy_from_slice(&3u16.to_le_bytes());
        // Byte order
        file[0x1C..0x1E].copy_from_slice(&0xFFFEu16.to_le_bytes());
        // Sector size power = 9 (512)
        file[0x1E..0x20].copy_from_slice(&9u16.to_le_bytes());
        // Mini sector size power = 6 (64)
        file[0x20..0x22].copy_from_slice(&6u16.to_le_bytes());
        // FAT sector count = 1
        file[0x2C..0x30].copy_from_slice(&1u32.to_le_bytes());
        // First directory sector = 0
        file[0x30..0x34].copy_from_slice(&0u32.to_le_bytes());
        // Mini-stream cutoff = 4096
        file[0x38..0x3C].copy_from_slice(&4096u32.to_le_bytes());
        // First mini-FAT sector = END_OF_CHAIN (no mini-FAT)
        file[0x3C..0x40].copy_from_slice(&END_OF_CHAIN.to_le_bytes());
        // Mini-FAT sector count = 0
        file[0x40..0x44].copy_from_slice(&0u32.to_le_bytes());
        // First DIFAT sector = END_OF_CHAIN (no DIFAT chain)
        file[0x44..0x48].copy_from_slice(&END_OF_CHAIN.to_le_bytes());
        // DIFAT sector count = 0
        file[0x48..0x4C].copy_from_slice(&0u32.to_le_bytes());
        // DIFAT[0] = sector 1 (FAT)
        file[0x4C..0x50].copy_from_slice(&1u32.to_le_bytes());
        // DIFAT[1..109] = FREE_SECT
        for i in 1..109 {
            let off = 0x4C + i * 4;
            file[off..off + 4].copy_from_slice(&FREE_SECT.to_le_bytes());
        }

        // ── Sector 0: Directory ──
        let dir_offset = 512;

        // Entry 0: Root Entry
        write_dir_entry(
            &mut file[dir_offset..dir_offset + 128],
            "Root Entry",
            5, // root storage
            1, // child = entry 1
            END_OF_CHAIN,
            0,
        );

        // Entry 1: "TestStream" (stream)
        write_dir_entry(
            &mut file[dir_offset + 128..dir_offset + 256],
            "TestStream",
            2, // stream
            NO_ENTRY,
            2,  // start sector = 2
            11, // size = 11 ("Hello, CFB!")
        );

        // Entry 2-3: Empty
        file[dir_offset + 256 + 0x42] = 0; // empty
        file[dir_offset + 384 + 0x42] = 0; // empty

        // ── Sector 1: FAT ──
        let fat_offset = 512 + sector_size;
        // Sector 0: END_OF_CHAIN (directory, single sector)
        write_fat_entry(&mut file, fat_offset, 0, END_OF_CHAIN);
        // Sector 1: FAT_SECT (this sector is a FAT sector)
        write_fat_entry(&mut file, fat_offset, 1, FAT_SECT);
        // Sector 2: END_OF_CHAIN (stream data)
        write_fat_entry(&mut file, fat_offset, 2, END_OF_CHAIN);
        // Rest: FREE_SECT
        for i in 3..128 {
            write_fat_entry(&mut file, fat_offset, i, FREE_SECT);
        }

        // ── Sector 2: Stream data ──
        let data_offset = 512 + 2 * sector_size;
        let stream_data = b"Hello, CFB!";
        file[data_offset..data_offset + stream_data.len()].copy_from_slice(stream_data);

        file
    }

    fn write_dir_entry(
        buf: &mut [u8],
        name: &str,
        entry_type: u8,
        child: u32,
        start_sector: u32,
        stream_size: u32,
    ) {
        let utf16: Vec<u16> = name.encode_utf16().collect();
        for (i, &ch) in utf16.iter().enumerate() {
            let bytes = ch.to_le_bytes();
            buf[i * 2] = bytes[0];
            buf[i * 2 + 1] = bytes[1];
        }
        let name_size = ((utf16.len() + 1) * 2) as u16;
        buf[0x40..0x42].copy_from_slice(&name_size.to_le_bytes());
        buf[0x42] = entry_type;
        buf[0x43] = 1; // black
        buf[0x44..0x48].copy_from_slice(&NO_ENTRY.to_le_bytes()); // left
        buf[0x48..0x4C].copy_from_slice(&NO_ENTRY.to_le_bytes()); // right
        buf[0x4C..0x50].copy_from_slice(&child.to_le_bytes());
        buf[0x74..0x78].copy_from_slice(&start_sector.to_le_bytes());
        buf[0x78..0x7C].copy_from_slice(&stream_size.to_le_bytes());
    }

    fn write_fat_entry(file: &mut [u8], fat_offset: usize, index: usize, value: u32) {
        let off = fat_offset + index * 4;
        file[off..off + 4].copy_from_slice(&value.to_le_bytes());
    }

    #[test]
    fn open_minimal_cfb() {
        let data = build_minimal_cfb();
        let cursor = Cursor::new(data);
        let reader = CfbReader::new(cursor).unwrap();
        assert_eq!(reader.header().major_version, 3);
        assert_eq!(reader.entries().len(), 4);
        assert_eq!(reader.entries()[0].name, "Root Entry");
        assert_eq!(reader.entries()[1].name, "TestStream");
    }

    #[test]
    fn read_stream_by_name() {
        let data = build_minimal_cfb();
        let cursor = Cursor::new(data);
        let mut reader = CfbReader::new(cursor).unwrap();
        let stream = reader.open_stream("TestStream").unwrap();
        assert_eq!(&stream, b"Hello, CFB!");
    }

    #[test]
    fn read_stream_case_insensitive() {
        let data = build_minimal_cfb();
        let cursor = Cursor::new(data);
        let mut reader = CfbReader::new(cursor).unwrap();
        let stream = reader.open_stream("teststream").unwrap();
        assert_eq!(&stream, b"Hello, CFB!");
    }

    #[test]
    fn stream_not_found() {
        let data = build_minimal_cfb();
        let cursor = Cursor::new(data);
        let mut reader = CfbReader::new(cursor).unwrap();
        assert!(reader.open_stream("NonExistent").is_err());
    }

    #[test]
    fn has_stream() {
        let data = build_minimal_cfb();
        let cursor = Cursor::new(data);
        let reader = CfbReader::new(cursor).unwrap();
        assert!(reader.has_stream("TestStream"));
        assert!(reader.has_stream("teststream"));
        assert!(!reader.has_stream("Missing"));
    }

    /// Build a CFB with a small stream that goes into the mini-stream.
    fn build_cfb_with_mini_stream() -> Vec<u8> {
        let sector_size = 512usize;
        // Layout:
        // Header (512)
        // Sector 0: Directory
        // Sector 1: FAT
        // Sector 2: Mini-stream container (Root Entry data, holds mini-stream data)
        // Sector 3: Mini-FAT
        let mut file = vec![0u8; 512 + 4 * sector_size];

        // ── Header ──
        file[0..8].copy_from_slice(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]);
        file[0x18..0x1A].copy_from_slice(&0x003Eu16.to_le_bytes());
        file[0x1A..0x1C].copy_from_slice(&3u16.to_le_bytes());
        file[0x1C..0x1E].copy_from_slice(&0xFFFEu16.to_le_bytes());
        file[0x1E..0x20].copy_from_slice(&9u16.to_le_bytes());
        file[0x20..0x22].copy_from_slice(&6u16.to_le_bytes());
        file[0x2C..0x30].copy_from_slice(&1u32.to_le_bytes());
        file[0x30..0x34].copy_from_slice(&0u32.to_le_bytes());
        file[0x38..0x3C].copy_from_slice(&4096u32.to_le_bytes());
        // First mini-FAT sector = 3
        file[0x3C..0x40].copy_from_slice(&3u32.to_le_bytes());
        file[0x40..0x44].copy_from_slice(&1u32.to_le_bytes()); // mini-FAT count = 1
        file[0x44..0x48].copy_from_slice(&END_OF_CHAIN.to_le_bytes());
        file[0x48..0x4C].copy_from_slice(&0u32.to_le_bytes());
        // DIFAT[0] = sector 1
        file[0x4C..0x50].copy_from_slice(&1u32.to_le_bytes());
        for i in 1..109 {
            let off = 0x4C + i * 4;
            file[off..off + 4].copy_from_slice(&FREE_SECT.to_le_bytes());
        }

        let dir_offset = 512;
        // Root Entry: start_sector=2 (mini-stream container), stream_size=512 (container size)
        write_dir_entry(
            &mut file[dir_offset..dir_offset + 128],
            "Root Entry",
            5,
            1,   // child = entry 1
            2,   // start sector (mini-stream container)
            512, // mini-stream container size
        );
        // Entry 1: "SmallStream" — small stream, goes to mini-stream
        // start_sector = 0 (mini-sector 0), size = 5
        write_dir_entry(
            &mut file[dir_offset + 128..dir_offset + 256],
            "SmallStream",
            2,
            NO_ENTRY,
            0, // start mini-sector
            5, // 5 bytes
        );
        // Empty entries
        file[dir_offset + 256 + 0x42] = 0;
        file[dir_offset + 384 + 0x42] = 0;

        // ── Sector 1: FAT ──
        let fat_offset = 512 + sector_size;
        write_fat_entry(&mut file, fat_offset, 0, END_OF_CHAIN); // dir
        write_fat_entry(&mut file, fat_offset, 1, FAT_SECT); // FAT
        write_fat_entry(&mut file, fat_offset, 2, END_OF_CHAIN); // mini-stream container
        write_fat_entry(&mut file, fat_offset, 3, END_OF_CHAIN); // mini-FAT
        for i in 4..128 {
            write_fat_entry(&mut file, fat_offset, i, FREE_SECT);
        }

        // ── Sector 2: Mini-stream container ──
        let ms_offset = 512 + 2 * sector_size;
        file[ms_offset..ms_offset + 5].copy_from_slice(b"Small");

        // ── Sector 3: Mini-FAT ──
        let mf_offset = 512 + 3 * sector_size;
        // Mini-sector 0: END_OF_CHAIN
        mf_offset_write(&mut file, mf_offset, 0, END_OF_CHAIN);
        for i in 1..128 {
            mf_offset_write(&mut file, mf_offset, i, FREE_SECT);
        }

        file
    }

    fn mf_offset_write(file: &mut [u8], base: usize, index: usize, value: u32) {
        let off = base + index * 4;
        file[off..off + 4].copy_from_slice(&value.to_le_bytes());
    }

    #[test]
    fn read_mini_stream() {
        let data = build_cfb_with_mini_stream();
        let cursor = Cursor::new(data);
        let mut reader = CfbReader::new(cursor).unwrap();
        let stream = reader.open_stream("SmallStream").unwrap();
        assert_eq!(&stream, b"Small");
    }

    #[test]
    fn find_entry_by_path_simple() {
        let data = build_minimal_cfb();
        let cursor = Cursor::new(data);
        let reader = CfbReader::new(cursor).unwrap();
        // "TestStream" is a child of root.
        let idx = reader.find_entry_by_path("TestStream");
        assert_eq!(idx, Some(1));
    }
}
