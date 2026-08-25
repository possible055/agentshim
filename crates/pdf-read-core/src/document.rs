//! PDF document model.

use crate::encryption::EncryptionHandler;
use crate::error::{Error, Result};
use crate::layout::TextSpan;
use crate::object::{Object, ObjectRef};
use crate::parser::parse_object;
use crate::parser_config::ParserOptions;
use crate::pipeline::{
    converters::OutputConverter, MarkdownOutputConverter, ReadingOrderContext, TextPipeline,
    TextPipelineConfig,
};
use crate::structure::traverse_structure_tree;
use crate::xref::{find_xref_offset, parse_xref, CrossRefTable};
use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader, Cursor, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::Mutex;

mod actualtext;
mod annotations_and_permissions;
mod assessment;
mod bidi_and_scripts;
mod block_ordering;
mod classification;
mod column_detection;
mod column_ordering;
mod embedded_font_widths;
mod font_identity;
mod font_loading;
mod image_and_font_public;
mod image_handles;
mod markdown;
mod object_loading;
mod object_streams_and_catalog;
mod opening_and_encryption;
mod page_content_api;
mod page_tree;
mod parsing;
mod preflight;
mod raw_spans;
mod region_extraction;
#[cfg(feature = "rendering")]
mod rendering;
mod rowspan_and_decryption;
mod sidebar_ordering;
mod span_extraction;
mod span_postprocessing;
mod span_spacing;
mod structure_and_pages;
mod structure_ordering;
mod tables_images_embedded;
mod text_assembly;
mod text_entry;
mod text_normalization;
mod vector_extraction;
mod warnings_and_gutters;
mod widget_spans;
mod word_and_line_extraction;

pub use parsing::{parse_header, parse_trailer, ExtractedImageRef, ImageFormat};
pub(crate) use preflight::ActualTextAction;

pub use assessment::{PageTextAssessment, PageTextStatus, PageVisualAssessment};

// Re-export MutexExt from cache module for local use and backward compatibility
pub(crate) use crate::cache::MutexExt;

/// Reading order mode for span extraction.
///
/// Controls how text spans are sorted after extraction from a PDF page.
/// The default `TopToBottom` mode uses simple geometric sorting, while
/// `ColumnAware` uses the XY-Cut algorithm to detect columns and read
/// each column top-to-bottom before moving to the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReadingOrder {
    /// Simple top-to-bottom, left-to-right ordering.
    ///
    /// Sorts spans by Y-coordinate descending (top of page first),
    /// then by X-coordinate ascending (left to right).
    #[default]
    TopToBottom,
    /// Column-aware ordering using the XY-Cut algorithm.
    ///
    /// Detects columns via projection-profile analysis and reads each
    /// column fully (top-to-bottom) before moving to the next column.
    /// Best for newspapers, academic papers, and multi-column layouts.
    ColumnAware,
    /// Logical-structure ordering from the document's `/StructTreeRoot`.
    ///
    /// For a Tagged PDF, ISO 32000-1:2008 §14.8.2.3 makes a pre-order traversal
    /// of the structure hierarchy AUTHORITATIVE for reading order - it is the
    /// producer's declared sequence, independent of glyph geometry, so it reads
    /// tables and complex layouts correctly where a geometric XY-cut guesses.
    ///
    /// Spans are ordered by their marked-content id (`/MCID`) following that
    /// traversal; any span without a matching MCID is appended in geometric
    /// (`ColumnAware`) order. When the structure tree is absent or not
    /// trustworthy for ordering (untagged, or `/Suspects true`), this falls back
    /// to `ColumnAware` entirely, so it is always safe to request.
    Structure,
}

enum PdfReader {
    File(BufReader<File>),
    Memory(BufReader<Cursor<Vec<u8>>>),
}

impl Read for PdfReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            PdfReader::File(r) => r.read(buf),
            PdfReader::Memory(r) => r.read(buf),
        }
    }
}

impl Seek for PdfReader {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        match self {
            PdfReader::File(r) => r.seek(pos),
            PdfReader::Memory(r) => r.seek(pos),
        }
    }
}

impl BufRead for PdfReader {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        match self {
            PdfReader::File(r) => r.fill_buf(),
            PdfReader::Memory(r) => r.fill_buf(),
        }
    }

    fn consume(&mut self, amt: usize) {
        match self {
            PdfReader::File(r) => r.consume(amt),
            PdfReader::Memory(r) => r.consume(amt),
        }
    }
}

/// Maximum recursion depth for object resolution
const MAX_RECURSION_DEPTH: u32 = 100;

/// Page information for rendering.
#[cfg(feature = "rendering")]
#[derive(Debug, Clone)]
pub struct PageInfo {
    /// Media box defining the page boundaries
    pub media_box: crate::geometry::Rect,
    /// Crop box if specified (for visible area)
    pub crop_box: Option<crate::geometry::Rect>,
    /// Page rotation in degrees (0, 90, 180, 270)
    pub rotation: i32,
}

/// Default maximum size in bytes for the object cache (64 MB).
///
/// This is a soft guardrail, not a hard ceiling. Real memory usage can be
/// 1.5–2× the cap because `estimate_size` does not account for HashMap bucket
/// overhead, Arc headers, or allocator padding.
const DEFAULT_OBJECT_CACHE_MAX_BYTES: usize = 64 * 1024 * 1024;

/// Default maximum number of entries for the XObject span/image caches.
const DEFAULT_XOBJECT_CACHE_MAX_ENTRIES: usize = 1024;

/// Ceiling for the decoded-XObject reuse cache when no call budget is installed.
///
/// Inside a call this is not the operative number — the call budget's stream-cache
/// ceiling is, and it is lower. This cache is the one stream allocation that survives
/// across pages, so leaving it outside the budget meant a call could hold this much
/// beyond everything the budget accounted for.
const DEFAULT_STREAM_CACHE_BYTES: usize = 50 * 1024 * 1024;

/// Admit a decoded XObject stream into the reuse cache if the budget has room for it.
///
/// Declining to cache is never an error: the caller already holds the decoded bytes and
/// only loses reuse on a later page.
pub(crate) fn admit_xobject_stream(
    document: &PdfDocument,
    xobject_ref: crate::object::ObjectRef,
    data: &[u8],
) {
    let room = crate::budget::stream_cache_room().min(DEFAULT_STREAM_CACHE_BYTES);
    let current = document.xobject_stream_cache_bytes.load(Ordering::Relaxed);
    let proposed = current.saturating_add(data.len());
    if data.len() > room || proposed > DEFAULT_STREAM_CACHE_BYTES {
        return;
    }
    if crate::budget::check_stream_cache_growth(proposed).is_err() {
        return;
    }
    document
        .xobject_stream_cache_bytes
        .store(proposed, Ordering::Relaxed);
    document
        .xobject_stream_cache
        .lock_or_recover()
        .insert(xobject_ref, std::sync::Arc::new(data.to_vec()));
}

/// Heuristic multiplier for the forward-gap guard in the main
/// assembly loop's compound newline predicate
/// (`y_diff > 2.0 && gap > K * max(fs)`). Visual gap-sweep over
/// synthetic two-column examples at fs=10 and fs=14 placed the
/// plausible operating band at roughly 0.7-1.5; 1.25 is a
/// conservative interim pick. Not corpus-calibrated; a page-level
/// layout signal would be a stronger long-term replacement for
/// this pairwise heuristic.
const FORWARD_GAP_K: f32 = 1.25;

/// Maximum allowed inter-span X gap inside a candidate same-line reorder run.
/// If the candidate's tentative X-order contains a larger gap, the run is
/// probably a disjoint footer/header/field layout rather than a local
/// mixed-baseline repair.
const SAME_LINE_REORDER_MAX_GAP_FACTOR: f32 = 3.0;

// Re-export BoundedEntryCache from cache module for local use and backward compatibility
pub(crate) use crate::cache::BoundedEntryCache;

/// Size-bounded object cache with FIFO eviction.
///
/// Wraps a `HashMap<ObjectRef, Object>` with byte-size tracking. When an
/// insertion would push total estimated size past `max_bytes`, the oldest
/// entries are evicted first (FIFO order via a `VecDeque` of keys).
///
/// FIFO is chosen over LRU because the access pattern is predominantly
/// insert-once-read-once — higher-level caches (font caches, xobject stream
/// cache) serve repeated lookups, so recency is not a useful signal here.
struct BoundedObjectCache {
    map: HashMap<ObjectRef, Object>,
    insertion_order: std::collections::VecDeque<ObjectRef>,
    current_bytes: usize,
    max_bytes: usize,
}

impl BoundedObjectCache {
    fn new(max_bytes: usize) -> Self {
        Self {
            map: HashMap::new(),
            insertion_order: std::collections::VecDeque::new(),
            current_bytes: 0,
            max_bytes,
        }
    }

    fn get(&self, key: &ObjectRef) -> Option<&Object> {
        self.map.get(key)
    }

    fn insert(&mut self, key: ObjectRef, value: Object) {
        let entry_size = Self::estimate_size(&value);

        // Don't cache objects that alone exceed the budget
        if entry_size > self.max_bytes {
            return;
        }

        // If the key already exists, subtract old size first
        if let Some(old_val) = self.map.get(&key) {
            self.current_bytes = self
                .current_bytes
                .saturating_sub(Self::estimate_size(old_val));
        }

        // Evict oldest entries until under budget. If the front of the
        // queue is the key we're about to (re)insert, skip past it so a
        // larger replacement doesn't leave the cache over budget — keep
        // evicting other entries instead.
        let mut skipped_self = false;
        while self.current_bytes + entry_size > self.max_bytes {
            match self.insertion_order.pop_front() {
                Some(old_key) => {
                    if old_key == key {
                        if skipped_self {
                            self.insertion_order.push_front(old_key);
                            break;
                        }
                        self.insertion_order.push_back(old_key);
                        skipped_self = true;
                        continue;
                    }
                    if let Some(old_val) = self.map.remove(&old_key) {
                        self.current_bytes = self
                            .current_bytes
                            .saturating_sub(Self::estimate_size(&old_val));
                    }
                }
                None => break,
            }
        }

        // Insert (or replace) the entry
        if self.map.insert(key, value).is_none() {
            // New key — track insertion order
            self.insertion_order.push_back(key);
        }
        self.current_bytes += entry_size;
        crate::metrics::record_object_cache(self.current_bytes);
        // Eviction already keeps the cache under `max_bytes`; this reports the call
        // budget's view, which may be tighter than the cache's own configured size.
        if crate::budget::check_cache_growth(self.current_bytes, self.map.len()).is_err() {
            self.evict_to(
                crate::budget::active_limits()
                    .map_or(self.max_bytes, |limits| limits.object_cache_bytes),
            );
        }
    }

    /// Drop oldest entries until the estimated live size is within `target`.
    fn evict_to(&mut self, target: usize) {
        while self.current_bytes > target {
            let Some(key) = self.insertion_order.pop_front() else {
                break;
            };
            if let Some(value) = self.map.remove(&key) {
                self.current_bytes = self
                    .current_bytes
                    .saturating_sub(Self::estimate_size(&value));
            }
        }
    }

    fn len(&self) -> usize {
        self.map.len()
    }

    fn keys(&self) -> impl Iterator<Item = &ObjectRef> {
        self.map.keys()
    }

    fn clear(&mut self) {
        self.map.clear();
        self.insertion_order.clear();
        self.current_bytes = 0;
    }

    fn estimate_size(obj: &Object) -> usize {
        Self::estimate_size_depth(obj, 8)
    }

    /// Rough estimate of an Object's heap size in bytes.
    /// Recurses into nested containers up to `depth` levels to avoid
    /// both underestimation and stack overflow on adversarial input.
    fn estimate_size_depth(obj: &Object, depth: u8) -> usize {
        if depth == 0 {
            return 64;
        }
        match obj {
            Object::Stream { dict, data } => {
                let dict_size: usize = dict
                    .iter()
                    .map(|(k, v)| k.len() + 32 + Self::estimate_size_depth(v, depth - 1))
                    .sum();
                data.len() + dict_size + 64
            }
            Object::Dictionary(d) => {
                let inner: usize = d
                    .iter()
                    .map(|(k, v)| k.len() + 32 + Self::estimate_size_depth(v, depth - 1))
                    .sum();
                inner + 64
            }
            Object::Array(a) => {
                let inner: usize = a
                    .iter()
                    .map(|v| Self::estimate_size_depth(v, depth - 1))
                    .sum();
                inner + 64
            }
            Object::String(s) => s.len() + 32,
            Object::Name(s) => s.len() + 32,
            _ => 32,
        }
    }
}

// Per-thread resolving stack and recursion depth for load_object.
// Thread-local storage avoids document-global lock contention and prevents
// false "circular reference" errors when two threads resolve the same object
// concurrently (#398 Race C).
thread_local! {
    static RESOLVING_STACK: RefCell<HashSet<ObjectRef>> = RefCell::new(HashSet::new());
    static RECURSION_DEPTH: RefCell<u32> = const { RefCell::new(0) };
}

/// PDF document.
///
/// This structure represents an open PDF document, providing access to:
/// - Document metadata (version, catalog, trailer)
/// - Page information (count, page tree)
/// - Object loading and dereferencing
///
/// # Example
///
/// ```no_run
/// use pdf_oxide::document::PdfDocument;
///
/// let mut doc = PdfDocument::open("sample.pdf")?;
/// println!("PDF version: {}.{}", doc.version().0, doc.version().1);
/// println!("Page count: {}", doc.page_count()?);
/// # Ok::<(), pdf_oxide::error::Error>(())
/// ```
///
/// # Memory management
///
/// The document maintains several internal caches for performance. The main
/// object cache is bounded at 64 MB (see `DEFAULT_OBJECT_CACHE_MAX_BYTES`)
/// uses FIFO eviction to prevent unbounded heap growth when processing
/// many pages sequentially.
pub struct PdfDocument {
    /// PDF reader — file-backed on native, memory-backed on WASM.
    ///
    /// # Thread Safety
    /// All interior-mutable fields use `Mutex` / `AtomicUsize`, making
    /// `PdfDocument` both `Send` and `Sync`.
    /// Wrapped in RefCell for interior mutability (seek/read require &mut).
    reader: Mutex<PdfReader>,
    /// Serializes concurrent *cold* (uncached) object loads on a shared
    /// handle. A single logical load makes many separate `reader` lock
    /// scopes (header, /Length resolution, stream bytes, nested refs);
    /// without this, two threads cold-loading on one shared `PdfDocument`
    /// (e.g. the C# binding's single native handle calling `render_page_fit`
    /// from multiple threads) interleave those scopes on the shared
    /// `BufReader` and read each other's bytes, surfacing as a spurious
    /// `[1000] invalid PDF structure or content stream`. Acquired only at
    /// the top-level entry of `load_object` (recursion depth 0) with a
    /// double-checked cache, so warm cache hits stay fully parallel
    /// same-thread recursion never re-acquires (no self-deadlock). #507.
    load_lock: Mutex<()>,
    /// PDF version (major, minor)
    version: (u8, u8),
    /// Cross-reference table mapping object IDs to byte offsets
    xref: CrossRefTable,
    /// Trailer dictionary
    trailer: Object,
    /// Cache for loaded objects to avoid re-parsing.
    /// Bounded at [`DEFAULT_OBJECT_CACHE_MAX_BYTES`] with FIFO eviction to
    /// prevent unbounded heap growth during multi-page extraction.
    object_cache: Mutex<BoundedObjectCache>,
    /// Encryption handler (if PDF is encrypted).
    /// Wrapped in RefCell for interior mutability (lazy initialization from &self).
    encryption_handler: Mutex<Option<EncryptionHandler>>,
    /// ObjectRef of the /Encrypt dictionary, cached so its strings are
    /// skipped during per-object string decryption. The entries in the
    /// encryption dict (/O, /U, /OE, /UE, /Perms, …) are key material used
    /// to derive the encryption key, not ciphertext, and must never be
    /// passed through `decrypt_string`.
    encrypt_dict_ref: Mutex<Option<ObjectRef>>,
    /// Parser configuration options for error handling and recovery
    #[allow(dead_code)]
    options: ParserOptions,
    /// Byte offset where PDF header was found (may not be 0 for malformed PDFs)
    #[allow(dead_code)]
    header_offset: u64,
    /// Font cache keyed by indirect ObjectRef to avoid re-parsing fonts across pages.
    /// Arc-wrapped to eliminate deep cloning when populating per-page TextExtractor.
    /// Bounded at 512 entries — TeX PDFs can create unique font objects per page.
    font_cache: Mutex<BoundedEntryCache<ObjectRef, Arc<crate::fonts::FontInfo>>>,
    /// Cached font sets keyed by /Font dictionary ObjectRef.
    /// Pages sharing the same /Font dict skip the entire load_fonts() loop.
    /// Bounded at 256 entries.
    font_set_cache: Mutex<BoundedEntryCache<ObjectRef, Vec<(String, Arc<crate::fonts::FontInfo>)>>>,
    /// Fingerprint-based font set cache for direct /Font dictionaries.
    /// Keyed by sorted font ObjectRefs hash, catches pages with different
    /// /Resources but same font references. Bounded at 256 entries.
    font_fingerprint_cache:
        Mutex<BoundedEntryCache<u64, Vec<(String, Arc<crate::fonts::FontInfo>)>>>,
    /// Name-based font set cache keyed by hash of sorted font names.
    /// Catches pages with different font ObjectRefs but the same font name→base font
    /// mapping (common in PDFs that create new font objects per page).
    /// Stores the resolved font set (Arc-wrapped to avoid cloning) plus a combined
    /// identity hash over ALL fonts for verification before reuse. Bounded at 256 entries.
    font_name_set_cache:
        Mutex<BoundedEntryCache<u64, (Arc<Vec<(String, Arc<crate::fonts::FontInfo>)>>, u64)>>,
    /// Per-font identity cache keyed by font_identity_hash (BaseFont + Subtype + Encoding +
    /// ToUnicode + FontDescriptor + DescendantFonts references). Skips expensive
    /// `FontInfo::from_dict()` when a structurally identical font was already parsed.
    /// Bounded at 512 entries.
    font_identity_cache: Mutex<BoundedEntryCache<u64, Arc<crate::fonts::FontInfo>>>,
    /// Per-object `font_identity_hash_cheap`, memoized. An object's content is
    /// fixed within a document, so the Layer-4 cache guard (#408) need not
    /// re-load and re-hash each font's `/Widths` on every page.
    font_id_hash_cache: Mutex<HashMap<ObjectRef, u64>>,
    /// Cached structure tree (None = not yet checked, Some(None) = untagged, Some(Some) = tagged).
    /// Uses Arc to avoid expensive deep clones on every page extraction.
    /// Mutex provides interior mutability for `&self` read-path methods (#398).
    structure_tree_cache: Mutex<Option<Option<Arc<crate::structure::StructTreeRoot>>>>,
    /// Cached per-page structure tree traversal results.
    /// Built once from the structure tree, then O(1) lookup per page.
    /// Mutex provides interior mutability for `&self` read-path methods (#398).
    structure_content_cache: Mutex<Option<HashMap<u32, Vec<crate::structure::OrderedContent>>>>,
    /// Cached resolved structure-tree `/ActualText` scopes.
    ///
    /// `None` = not yet built, `Some(None)` = built and the document has
    /// no resolvable ActualText (untagged, or every bearing element
    /// dropped during finalisation), `Some(Some(idx))` = built.
    ///
    /// Mirrors `structure_tree_cache` so every extraction surface
    /// applies tree-scope ActualText consistently without re-walking the
    /// structure tree. Decoupled from `/MarkInfo /Suspects`: producer-
    /// supplied ActualText is trusted regardless of Suspects (it is
    /// content replacement, not reading order — see
    /// `actualtext_index`).
    actualtext_index_cache: Mutex<Option<Option<Arc<crate::structure::ActualTextIndex>>>>,
    /// Per-page set of MCIDs whose marked-content sequence carried an
    /// inline `/ActualText` property (ISO 32000-1:2008 §14.6).
    ///
    /// Populated by `extract_spans_impl` from the text extractor's
    /// per-call detection: the per-page entry is REPLACED on each
    /// extraction so MC-scope precedence reflects the latest run, not
    /// stale data from an earlier filter set.
    ///
    /// The struct-tree-scope ActualText applier consults this set to
    /// enforce the precedence rule: the MC-scope (inline) replacement
    /// is the innermost and most specific declaration for the MCID
    /// it covers, so a struct-tree-scope `/ActualText` on an ancestor
    /// element must NOT override it.
    pub(crate) mc_actualtext_mcids: Mutex<HashMap<usize, HashSet<u32>>>,
    /// `Table` structure elements bucketed by page, built once via
    /// `find_table_elements_all_pages` (one tree walk) so the converter table
    /// path does an O(1) lookup instead of walking the tree per page.
    /// `None` = not yet built.
    table_elements_cache: Mutex<Option<HashMap<u32, Vec<crate::structure::StructElem>>>>,
    /// Page object cache keyed by page index to avoid re-traversing the page tree.
    /// The page tree structure is static (§7.7.3.2), so pages can be safely cached.
    /// Mutex provides interior mutability for `&self` read-path methods (#398).
    page_cache: Mutex<HashMap<usize, Object>>,
    /// Whether the bulk page tree walk has been attempted (successful or not).
    /// Prevents re-walking the tree on every cache miss for malformed PDFs.
    page_cache_populated: AtomicBool,
    /// Cached object offsets from full file scan (built on first xref miss).
    /// Maps object number to byte offset in file.
    scanned_object_offsets: Mutex<Option<HashMap<u32, u64>>>,
    /// Whether the one-time object-stream recovery sweep has been attempted.
    /// See `recover_from_object_streams`. Separate from the scanned offsets
    /// cache because the sweep is only triggered on free-entry misses that
    /// also failed the file-body scan — the common path never needs it.
    objstm_recovery_done: Mutex<bool>,
    /// Cache of XObject refs known to NOT be Form XObjects (i.e., Image or unknown).
    /// Used by text extraction to skip expensive full-object loads for images.
    image_xobject_cache: Mutex<HashSet<ObjectRef>>,
    /// Document-level cache of Form XObject refs whose streams contain NO text
    /// operators (BT) and no nested Do invocations. Persists across pages so that
    /// shared graphics-only XObjects (watermarks, logos, chart elements) are
    /// decompressed and scanned at most once across the entire document.
    pub(crate) xobject_text_free_cache: Mutex<HashSet<ObjectRef>>,
    /// Cache of decompressed Form XObject streams. Bounded at 50MB total.
    /// Avoids repeated FlateDecode decompression of shared Form XObjects.
    pub(crate) xobject_stream_cache: Mutex<HashMap<ObjectRef, std::sync::Arc<Vec<u8>>>>,
    pub(crate) xobject_stream_cache_bytes: AtomicUsize,
    /// Cache of extracted TextSpan results from self-contained Form XObjects
    /// (those with own /Resources/Font). None = processed but no spans.
    /// Key is `(ObjectRef, [i64; 6])` where the array encodes the caller's CTM
    /// as millipoint-rounded integers, allowing the same Form XObject to cache
    /// distinct results for each unique CTM it is painted with.
    /// Bounded at [`DEFAULT_XOBJECT_CACHE_MAX_ENTRIES`] entries with FIFO eviction.
    pub(crate) xobject_spans_cache:
        Mutex<BoundedEntryCache<(ObjectRef, [i64; 6]), Option<Vec<crate::layout::TextSpan>>>>,
    /// Cache of extracted images from Form XObjects (keyed by ObjectRef).
    /// Images are stored without CTM applied — caller applies its own CTM.
    /// Bounded at [`DEFAULT_XOBJECT_CACHE_MAX_ENTRIES`] entries with FIFO eviction.
    pub(crate) form_xobject_images_cache:
        Mutex<BoundedEntryCache<ObjectRef, Vec<crate::extractors::PdfImage>>>,
    /// LRU cache of decompressed page content streams, keyed by page index.
    page_content_cache: Mutex<BoundedEntryCache<usize, std::sync::Arc<Vec<u8>>>>,
    /// LRU cache of postprocessed [`TextSpan`]s per page. `to_markdown`/`to_html`
    /// reach `extract_spans` twice per page — once directly, once via
    /// `extract_page_tables` → `extract_words` → `page_reading_order`; this serves
    /// the second from cache. Cleared by redaction (`erase_region` /
    /// `clear_erase_regions`), the only span-affecting mutation.
    page_spans_cache: Mutex<BoundedEntryCache<usize, std::sync::Arc<Vec<crate::layout::TextSpan>>>>,
    /// Per-page character cache for the unfiltered (`extract_chars`) result.
    /// `postprocess_spans` needs the same char sequence the public API returns,
    /// so without this every span extraction re-parses the content stream a
    /// second time purely to stamp per-glyph x-origins.
    page_chars_cache: Mutex<BoundedEntryCache<usize, std::sync::Arc<Vec<crate::layout::TextChar>>>>,
    /// Cached signatures of running headers/footers detected via cross-page
    /// repetition. A span whose normalized text matches a signature
    /// sits near the top/bottom of the page is treated as an artifact.
    /// Populated lazily on first access; `Some(set)` with an empty set
    /// means detection ran and found nothing (vs `None` = not yet run).
    /// Signatures of running headers/footers plus the first page index where
    /// each signature was observed. Used to mark repeat occurrences as
    /// pagination artifacts while keeping the first appearance intact — the
    /// first appearance is often the document's cover-page title that just
    /// happens to echo into the header band on every page (B3: pdfa_010
    /// would otherwise drop "University of Oklahoma 2009").
    running_artifact_signatures:
        Mutex<Option<std::sync::Arc<std::collections::HashMap<String, usize>>>>,
    /// Document-wide article threads (`/Threads`), parsed once. Reading-order
    /// resolution consults them per page, and parsing walks the whole page
    /// tree — so without this the cost per page scaled with the document.
    article_threads_cache: Mutex<Option<std::sync::Arc<Vec<crate::structure::ArticleThread>>>>,
    /// Memoised result of [`PdfDocument::output_intent_cmyk_profile`].
    ///
    /// The accessor walks `/OutputIntents` and decodes + parses the ICC
    /// stream every call. The hot transparency / overprint paths invoke
    /// it once per paint and the parse is non-trivial (qcms / lcms2
    /// header validation + LUT decode on a profile blob that can be
    /// hundreds of KB), so the result is cached for the document
    /// lifetime here. `Some(None)` means "checked once, no usable CMYK
    /// OutputIntent" — distinct from `None` (not yet checked).
    output_intent_cmyk_profile_cache:
        Mutex<Option<Option<std::sync::Arc<crate::color::IccProfile>>>>,
    /// Accumulated extraction warnings for programmatic inspection.
    /// Populated when silent fallbacks occur (font not found, CMap absent, etc.).
    /// Retrieve with [`PdfDocument::warnings`]; drain with [`PdfDocument::take_warnings`].
    accumulated_warnings: Mutex<Vec<String>>,
    /// structured warnings accumulator. Each
    /// internal warning site that previously only called `log::warn!`
    /// can additionally push a typed [`crate::extractors::warnings::Warning`]
    /// here, letting callers retrieve diagnostics as structured data
    /// (via [`PdfDocument::structured_warnings`]) instead of parsing
    /// stderr text. The existing String-list `accumulated_warnings`
    /// stays for back-compat.
    warning_sink: std::sync::Arc<crate::extractors::warnings::WarningSink>,
}

// Compile-time verification that PdfDocument is Send + Sync.
const _: () = {
    fn _assert_send_sync<T: Send + Sync>() {}
    fn _check() {
        _assert_send_sync::<PdfDocument>();
    }
};

impl std::fmt::Debug for PdfDocument {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PdfDocument")
            .field("version", &self.version)
            .field("xref_entries", &self.xref.len())
            .field("cached_objects", &self.object_cache.lock_or_recover().len())
            .finish_non_exhaustive()
    }
}

/// Pre-decompression filter for image extraction.
///
/// Dimensions are checked against XObject dictionary metadata (Width, Height,
/// ColorSpace) BEFORE the stream is decompressed, avoiding expensive decoding
/// of images that will be discarded downstream.
struct ImageExtractFilter {
    /// Minimum width in pixels (images narrower are skipped).
    min_width: i64,
    /// Minimum height in pixels (images shorter are skipped).
    min_height: i64,
    /// Maximum total pixels (images exceeding this are skipped).
    max_pixels: u64,
    /// Skip Indexed-colorspace images below this dimension.
    /// 0 means disabled.
    skip_indexed_small: i64,
}

impl Default for ImageExtractFilter {
    fn default() -> Self {
        Self {
            min_width: 8,
            min_height: 8,
            max_pixels: u64::MAX,
            skip_indexed_small: 0,
        }
    }
}

/// Default max image pixels for markdown/HTML embedding (16 MP).
/// Covers A4 at 300 DPI (8.7 MP) with comfortable margin.
const DEFAULT_MAX_IMAGE_PIXELS: u64 = 16_000_000;

impl ImageExtractFilter {
    /// Strict filter for markdown/HTML embedding paths.
    ///
    /// Skips tiny glyph fragments (<32x32), small Indexed images (<64x64),
    /// and oversized images beyond the configured limit. The `max_pixels`
    /// override comes from `ConversionOptions::max_image_pixels`.
    fn markdown(max_pixels_override: Option<u64>) -> Self {
        Self {
            min_width: 32,
            min_height: 32,
            max_pixels: max_pixels_override.unwrap_or(DEFAULT_MAX_IMAGE_PIXELS),
            skip_indexed_small: 64,
        }
    }
}
