//! Cross-reference table parser.
//!
//! The xref table maps object numbers to byte offsets in the PDF file,
//! enabling random access to PDF objects.
//!
//! Supports both traditional xref tables (PDF 1.0-1.4) and
//! cross-reference streams (PDF 1.5+).

use crate::error::{Error, Result};
use crate::object::Object;
use crate::parser::parse_object;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};

/// Cross-reference table entry type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XRefEntryType {
    /// Entry for a free object
    Free,
    /// Entry for an uncompressed object (traditional)
    Uncompressed,
    /// Entry for an object in an object stream (PDF 1.5+)
    Compressed,
}

/// Cross-reference table entry.
///
/// Each entry contains information about where to find an object.
/// Supports both traditional entries (byte offset) and compressed entries
/// (object stream reference).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XRefEntry {
    /// Type of entry
    pub entry_type: XRefEntryType,
    /// Byte offset (for uncompressed) or object stream number (for compressed)
    pub offset: u64,
    /// Generation number (for uncompressed) or index within stream (for compressed)
    pub generation: u16,
    /// Whether the object is in use (for traditional entries only)
    pub in_use: bool,
}

impl XRefEntry {
    /// Create a new cross-reference entry (traditional format).
    pub fn new(offset: u64, generation: u16, in_use: bool) -> Self {
        Self {
            entry_type: if in_use {
                XRefEntryType::Uncompressed
            } else {
                XRefEntryType::Free
            },
            offset,
            generation,
            in_use,
        }
    }

    /// Create a new uncompressed entry.
    pub fn uncompressed(offset: u64, generation: u16) -> Self {
        Self {
            entry_type: XRefEntryType::Uncompressed,
            offset,
            generation,
            in_use: true,
        }
    }

    /// Create a new compressed entry (object in object stream).
    pub fn compressed(stream_obj_num: u64, index_in_stream: u16) -> Self {
        Self {
            entry_type: XRefEntryType::Compressed,
            offset: stream_obj_num,
            generation: index_in_stream,
            in_use: true,
        }
    }

    /// Create a new free entry.
    pub fn free(next_free: u64, generation: u16) -> Self {
        Self {
            entry_type: XRefEntryType::Free,
            offset: next_free,
            generation,
            in_use: false,
        }
    }
}

/// Cross-reference table that maps object numbers to their locations.
#[derive(Debug, Clone)]
pub struct CrossRefTable {
    pub(crate) entries: HashMap<u32, XRefEntry>,
    /// Trailer dictionary (for xref streams, this is the stream dictionary)
    trailer: Option<HashMap<String, Object>>,
}

impl CrossRefTable {
    /// Create a new empty cross-reference table.
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            trailer: None,
        }
    }

    /// Set the trailer dictionary.
    pub fn set_trailer(&mut self, trailer: HashMap<String, Object>) {
        self.trailer = Some(trailer);
    }

    /// Get the trailer dictionary if present.
    pub fn trailer(&self) -> Option<&HashMap<String, Object>> {
        self.trailer.as_ref()
    }

    /// Add an entry to the cross-reference table.
    pub fn add_entry(&mut self, object_number: u32, entry: XRefEntry) {
        self.entries.insert(object_number, entry);
    }

    /// Get an entry by object number.
    pub fn get(&self, object_number: u32) -> Option<&XRefEntry> {
        self.entries.get(&object_number)
    }

    /// Check if an object exists in the xref table.
    pub fn contains(&self, object_number: u32) -> bool {
        self.entries.contains_key(&object_number)
    }

    /// Get all object numbers in the table.
    pub fn all_object_numbers(&self) -> impl Iterator<Item = u32> + '_ {
        self.entries.keys().copied()
    }

    /// The `max` smallest **in-use** object numbers, ascending.
    ///
    /// `entries` is a `HashMap`, so iteration order is nondeterministic; a
    /// bounded scan over an arbitrary subset can miss the target. Selecting
    /// the smallest in-use numbers makes scans deterministic and prioritizes
    /// low-numbered live objects (where the Catalog conventionally lives).
    /// Free entries are excluded so a long low-numbered free list can't
    /// crowd the bounded set. A bounded max-heap keeps this O(n log max)
    /// time / O(max) memory rather than sorting all n on a pathological or
    /// maliciously sparse xref.
    pub(crate) fn smallest_object_numbers(&self, max: usize) -> Vec<u32> {
        if max == 0 {
            return Vec::new();
        }
        let mut heap: std::collections::BinaryHeap<u32> =
            std::collections::BinaryHeap::with_capacity(max + 1);
        // Only live objects are scan candidates. Traditional xref tables
        // store free entries (the free list); a file with more than `max`
        // low-numbered *free* objects would otherwise exhaust the bounded
        // candidate set before any live Catalog is even considered.
        for (&n, e) in self.entries.iter() {
            if !e.in_use {
                continue;
            }
            heap.push(n);
            if heap.len() > max {
                heap.pop(); // drop the current largest
            }
        }
        heap.into_sorted_vec()
    }

    /// Merge entries from another xref table.
    ///
    /// Entries in self override entries in other (for incremental updates).
    /// This is used when following /Prev pointers in the trailer.
    pub fn merge_from(&mut self, other: CrossRefTable) {
        // Add entries from other that don't exist in self
        for (obj_num, entry) in other.entries {
            self.entries.entry(obj_num).or_insert(entry);
        }

        // If self doesn't have a trailer but other does, use other's trailer
        if self.trailer.is_none() && other.trailer.is_some() {
            self.trailer = other.trailer;
        }
    }

    /// Get the number of entries in the table.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Check if the table is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Shift all uncompressed entry offsets by a delta.
    ///
    /// Used when a PDF has garbage bytes prepended before `%PDF-`:
    /// the xref offsets are relative to the real start of the PDF data,
    /// but byte positions in the file are shifted by `header_offset`.
    pub fn shift_offsets(&mut self, delta: u64) {
        for entry in self.entries.values_mut() {
            if entry.in_use && entry.entry_type == XRefEntryType::Uncompressed {
                entry.offset += delta;
            }
        }
    }
}

impl Default for CrossRefTable {
    fn default() -> Self {
        Self::new()
    }
}

mod parser;
mod stream;

use parser::find_stream_length;
pub use parser::{find_xref_offset, parse_xref};
use stream::{parse_xref_stream, read_until_trailer, split_lines};

#[cfg(test)]
mod tests;
